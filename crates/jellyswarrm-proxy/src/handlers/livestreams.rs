use axum::{extract::State, Json};
use hyper::StatusCode;
use serde_json::Value;

use crate::{
    extractors::CatalogPreprocessed,
    handlers::{
        common::{execute_processed_json_request, track_play_session},
        media_versions::record_playback_sources,
    },
    models::MediaSource,
    processors::{response_processor::ResponseProcessingProfile, url_processor::play_session_id},
    request_preprocessing::{apply_to_request, remap_authorization},
    AppState,
};

/// OpenToken is backend-local. Use the caller-owned negotiation binding, and
/// retain its exact member even when an aggregate has several copies per server.
pub async fn post_livestream_open(
    State(state): State<AppState>,
    CatalogPreprocessed(preprocessed): CatalogPreprocessed,
) -> Result<Json<Value>, StatusCode> {
    let scope = preprocessed
        .access_scope
        .as_ref()
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let session_id = play_session_id(preprocessed.original_request.url())
        .map_err(|_| StatusCode::BAD_REQUEST)?
        .ok_or(StatusCode::BAD_REQUEST)?;
    let (binding, _) = state
        .play_sessions
        .resolve_report_session(&session_id, scope.user_id())
        .await
        .map_err(|_| StatusCode::FORBIDDEN)?
        .ok_or(StatusCode::BAD_REQUEST)?;
    let item_ids = preprocessed
        .original_request
        .url()
        .query_pairs()
        .filter(|(key, _)| key.eq_ignore_ascii_case("ItemId"))
        .map(|(_, value)| crate::media_storage_service::MediaStorageService::normalize_uuid(&value))
        .collect::<Vec<_>>();
    let item_id = item_ids.first().ok_or(StatusCode::BAD_REQUEST)?;
    if item_ids.iter().any(|id| id != item_id)
        || !state
            .play_sessions
            .get_sessions_by_item_id(item_id)
            .await
            .iter()
            .any(|session| session.session_id == session_id && session.user_id == scope.user_id())
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    if !scope.allows(binding.server_id) {
        return Err(StatusCode::FORBIDDEN);
    }
    let (session, server) = preprocessed
        .sessions
        .as_ref()
        .and_then(|sessions| {
            sessions
                .iter()
                .find(|(_, server)| server.id == binding.server_id)
        })
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let original_item_id = binding
        .original_item_id
        .as_deref()
        .ok_or(StatusCode::BAD_REQUEST)?;
    let generation = state
        .media_storage
        .begin_media_reconciliation()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut request = preprocessed
        .original_request
        .try_clone()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let pairs = request
        .url()
        .query_pairs()
        .filter(|(key, _)| {
            !["ItemId", "PlaySessionId", "SessionId"]
                .iter()
                .any(|field| key.eq_ignore_ascii_case(field))
        })
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    request
        .url_mut()
        .query_pairs_mut()
        .clear()
        .extend_pairs(pairs)
        .append_pair("ItemId", original_item_id)
        .append_pair("PlaySessionId", &session_id);
    let session = Some(session.clone());
    let auth = remap_authorization(&preprocessed.auth, &session)
        .await
        .map_err(|_| StatusCode::UNAUTHORIZED)?;
    apply_to_request(&mut request, server, &session, &auth, &state, Some(scope)).await;
    let token = preprocessed.auth.as_ref().and_then(|auth| auth.token_ref());
    let mut response = execute_processed_json_request(
        &state,
        request,
        server,
        ResponseProcessingProfile::Media,
        token,
    )
    .await?;
    // Jellyfin returns a singular MediaSource here, not a PlaybackResponse.
    let source: MediaSource = serde_json::from_value(response["MediaSource"].clone())
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    let mut sources = vec![source];
    record_playback_sources(
        &state,
        item_id,
        server,
        original_item_id,
        generation,
        &mut sources,
    )
    .await?;
    let source = sources.first().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    track_play_session(source, &session_id, scope.user_id(), server, &state).await?;
    state
        .play_sessions
        .bind_original_item(&session_id, scope.user_id(), server.id, original_item_id)
        .await;
    response["MediaSource"] =
        serde_json::to_value(source).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(response))
}
