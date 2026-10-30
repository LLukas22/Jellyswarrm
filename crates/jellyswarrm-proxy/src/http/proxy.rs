//! Handle fallback forwarding, translating preprocessing failures into safe HTTP responses.
use crate::{
    application, http::state::MediaState, processors::response_processor::ResponseProcessingProfile,
};
use axum::{
    body::Body,
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use percent_encoding::percent_decode_str;
use tracing::error;
// Only expose actionable validation messages; database and transport details stay in logs.
pub(crate) fn client_request_error(error: &anyhow::Error) -> &'static str {
    let message = error.to_string();
    if message.contains("mixed-server playlists are unsupported") {
        "Playlist track or entry is unavailable on the selected server; mixed-server playlists are unsupported"
    } else if message.contains("mixed-server collections are unsupported") {
        "Collection item is unavailable on the selected server; mixed-server collections are unsupported"
    } else if message.contains("Requested user has no mapping on the selected server") {
        "Requested user has no mapping on the selected server"
    } else {
        "Invalid proxy request"
    }
}

#[axum::debug_handler]
pub(crate) async fn proxy_handler(
    State(state): State<MediaState>,
    req: Request,
) -> Result<Response<Body>, StatusCode> {
    // Session/control routes never fall through, including encoded or mixed-case paths.
    let path_without_prefix = state.paths.remove_prefix(req.uri().path());
    let decoded = percent_decode_str(path_without_prefix).decode_utf8_lossy();
    let response_profile =
        ResponseProcessingProfile::for_proxy_request(req.method(), path_without_prefix);
    let action = crate::processors::request_analyzer::playback_session_action(
        req.method(),
        path_without_prefix,
    );
    let is_session_path = decoded
        .split('/')
        .find(|part| !part.is_empty())
        .is_some_and(|part| part.eq_ignore_ascii_case("sessions"));
    if is_session_path && action.is_none() {
        return Err(StatusCode::NOT_IMPLEMENTED);
    }
    if let Some(response) = super::assets::serve(&req) {
        return response;
    }

    let prepared = async {
        let request = super::preprocessing::extract_request_infos(req, &state, action).await?;
        application::proxy::prepare(&state.proxy_service(), request, action).await
    }
    .await;
    let prepared = match prepared {
        Ok(request) => request,
        Err(error) => {
            error!("Failed to preprocess request: {}", error);
            let message = client_request_error(&error);
            let error = application::input::routing_error(error);
            let status = super::error::status(&error);
            return Ok((status, message).into_response());
        }
    };

    let response =
        match application::proxy::forward(&state.proxy_service(), prepared, response_profile).await
        {
            Ok(response) => response,
            Err(application::error::ApplicationError::RequestBody(cause)) => {
                return Ok((StatusCode::BAD_REQUEST, client_request_error(&cause)).into_response())
            }
            Err(error) => return Err(super::error::status(&error)),
        };
    Ok(response.map(Body::from))
}
