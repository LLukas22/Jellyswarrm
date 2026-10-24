//! Handle fallback forwarding, translating preprocessing failures into safe HTTP responses.
use crate::{application, processors::response_processor::ResponseProcessingProfile, AppState};
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
    State(state): State<AppState>,
    req: Request,
) -> Result<Response<Body>, StatusCode> {
    // Session/control routes never fall through, including encoded or mixed-case paths.
    let path_without_prefix = state.remove_prefix_from_path(req.uri().path()).await;
    let decoded = percent_decode_str(path_without_prefix).decode_utf8_lossy();
    let response_profile =
        ResponseProcessingProfile::for_proxy_request(req.method(), path_without_prefix);
    let is_session_path = decoded
        .split('/')
        .find(|part| !part.is_empty())
        .is_some_and(|part| part.eq_ignore_ascii_case("sessions"));
    if is_session_path
        && crate::http::preprocessing::playback_session_action(
            req.method(),
            req.uri().path(),
            &state,
        )
        .await
        .is_none()
    {
        return Err(StatusCode::NOT_IMPLEMENTED);
    }
    if let Some(response) = super::assets::serve(&req) {
        return response;
    }

    let prepared = async {
        let action = crate::http::preprocessing::playback_session_action(
            req.method(),
            req.uri().path(),
            &state,
        )
        .await;
        let crate::application::input::CollectedRequest {
            original_request,
            auth,
            user,
            sessions,
            body_analysis,
            access_scope,
        } = crate::http::preprocessing::extract_request_infos(req, &state, action).await?;
        let context = application::context::RequestContext::new(
            &original_request,
            auth,
            user,
            sessions,
            access_scope,
        );
        application::proxy::prepare(
            &state,
            application::input::OperationRequest {
                original_request,
                context,
                body_analysis,
            },
            action,
        )
        .await
    }
    .await;
    let prepared = match prepared {
        Ok(request) => request,
        Err(error) => {
            error!("Failed to preprocess request: {}", error);
            let status = if error.is::<crate::http::preprocessing::NoAvailableBackend>() {
                StatusCode::SERVICE_UNAVAILABLE
            } else if error.is::<crate::http::preprocessing::MediaAccessDenied>() {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::BAD_REQUEST
            };
            return Ok((status, client_request_error(&error)).into_response());
        }
    };

    let response = match application::proxy::forward(&state, prepared, response_profile).await {
        Ok(response) => response,
        Err(application::error::ApplicationError::RequestBody(cause)) => {
            return Ok((StatusCode::BAD_REQUEST, client_request_error(&cause)).into_response())
        }
        Err(error) => return Err(error.detail_status()),
    };
    Ok(response.map(Body::from))
}
