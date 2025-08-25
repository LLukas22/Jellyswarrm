use axum::extract::{Request, State};
use hyper::StatusCode;
use reqwest::Body;
use tracing::{debug, error};

use crate::{
    handlers::common::payload_from_request,
    models::ProgressRequest,
    request_preprocessing::{apply_to_request, extract_request_infos, remap_authorization},
    AppState,
};

// http://localhost:3000/Sessions/Playing
// http://localhost:3000/Sessions/Playing/Progress
// http://localhost:3000/Sessions/Playing/Stopped
pub async fn post_playing(
    State(state): State<AppState>,
    req: Request,
) -> Result<StatusCode, StatusCode> {
    let (request, auth, _, sessions) = extract_request_infos(req, &state).await.map_err(|e| {
        error!("Failed to preprocess request: {}", e);
        StatusCode::BAD_REQUEST
    })?;

    let sessions = sessions.ok_or(StatusCode::UNAUTHORIZED)?;

    let mut payload: ProgressRequest = payload_from_request(&request)?;

    let session_server = if let Some((media_mapping, server)) = state
        .media_storage
        .get_media_mapping_with_server(&payload.media_source_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        payload.media_source_id = media_mapping.original_media_id.clone();
        payload.item_id = media_mapping.original_media_id.clone();

        if let Some(now_playing_queue) = payload.now_playing_queue.as_mut() {
            now_playing_queue.iter_mut().for_each(|item| {
                item.id = media_mapping.original_media_id.clone();
            });
        }
        server
    } else {
        error!(
            "No server found for media source: {}",
            payload.media_source_id
        );
        return Err(StatusCode::NOT_FOUND);
    };

    let session = if let Some((session, server)) = sessions.iter().find(|(_, server)| {
        let request_url = session_server.url.as_str().trim_end_matches('/');
        let server_url = server.url.as_str().trim_end_matches('/');
        request_url == server_url
    }) {
        debug!(
            "Reporting Progress to server: {} ({})",
            server.name, server.url
        );
        Some(session.clone())
    } else {
        error!("No user session found for server: {}", session_server.url);
        return Err(StatusCode::UNAUTHORIZED);
    };

    let new_auth = remap_authorization(&auth, &session).await.map_err(|e| {
        error!("Failed to process auth: {}", e);
        StatusCode::UNAUTHORIZED
    })?;

    let mut request = request;

    apply_to_request(&mut request, &session_server, &session, &new_auth, &state).await;

    let json = serde_json::to_vec(&payload).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    *request.body_mut() = Some(Body::from(json));

    let response = state.reqwest_client.execute(request).await.map_err(|e| {
        error!("Failed to execute request: {}", e);
        StatusCode::BAD_GATEWAY
    })?;

    let status = response.status();
    if !status.is_success() {
        error!("Request failed with status: {}", status);
    }
    Ok(status)
}
