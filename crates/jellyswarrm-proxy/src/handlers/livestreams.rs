use axum::response::{IntoResponse, Response};
use axum::{extract::State, Json};
use hyper::StatusCode;
use serde_json::Value;

use crate::{
    extractors::CatalogPreprocessed,
    handlers::{
        common::{execute_processed_json_request, set_json_body, track_play_session},
        media_versions::record_playback_sources,
    },
    models::MediaSource,
    processors::{response_processor::ResponseProcessingProfile, url_processor::play_session_id},
    request_preprocessing::{apply_to_request, remap_authorization},
    AppState,
};

fn body(request: &reqwest::Request) -> Result<Value, StatusCode> {
    match request
        .body()
        .and_then(|body| body.as_bytes())
        .filter(|bytes| !bytes.is_empty())
    {
        Some(bytes) => serde_json::from_slice(bytes).map_err(|_| StatusCode::BAD_REQUEST),
        None => Ok(serde_json::json!({})),
    }
}

// Jellyfin gives query parameters precedence over the corresponding DTO field.
fn field(
    request: &reqwest::Request,
    body: &Value,
    name: &str,
) -> Result<Option<String>, StatusCode> {
    let values = request
        .url()
        .query_pairs()
        .filter(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.into_owned())
        .collect::<Vec<_>>();
    if let Some(first) = values.first() {
        if values.iter().any(|value| value != first) {
            return Err(StatusCode::BAD_REQUEST);
        }
        return Ok(Some(first.clone()));
    }
    let object = body.as_object().ok_or(StatusCode::BAD_REQUEST)?;
    let mut result = None;
    for (_, value) in object
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(name))
    {
        if value.is_null() {
            continue;
        }
        let value = value.as_str().ok_or(StatusCode::BAD_REQUEST)?;
        if result.as_deref().is_some_and(|previous| previous != value) {
            return Err(StatusCode::BAD_REQUEST);
        }
        result = Some(value.to_string());
    }
    Ok(result)
}

/// Virtualize opaque live IDs and their delivery URL references together.
pub(crate) async fn bind_live_source(
    state: &AppState,
    source: &mut MediaSource,
    user: &str,
    server: &crate::server_storage::Server,
) -> Result<(), StatusCode> {
    let mut value =
        serde_json::to_value(&*source).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let Some(original) = value
        .as_object()
        .and_then(|object| {
            object
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case("LiveStreamId"))
        })
        .and_then(|(_, value)| value.as_str())
        .filter(|id| !id.is_empty())
        .map(str::to_string)
    else {
        return Ok(());
    };
    let id = state
        .play_sessions
        .register_live_stream(&original, user, server.id)
        .await;
    rewrite_live_id(&mut value, &id);
    *source = serde_json::from_value(value).map_err(|_| StatusCode::BAD_GATEWAY)?;
    Ok(())
}

fn rewrite_live_id(value: &mut Value, id: &str) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if key.eq_ignore_ascii_case("LiveStreamId") {
                    *value = Value::String(id.into());
                } else if ["TranscodingUrl", "StreamUrl", "DeliveryUrl"]
                    .iter()
                    .any(|field| key.eq_ignore_ascii_case(field))
                {
                    if let Some(url) = value.as_str() {
                        if let Some((path, query)) = url.split_once('?') {
                            let mut pairs = url::form_urlencoded::Serializer::new(String::new());
                            for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
                                pairs.append_pair(
                                    &key,
                                    if key.eq_ignore_ascii_case("LiveStreamId") {
                                        id
                                    } else {
                                        &value
                                    },
                                );
                            }
                            *value = Value::String(format!("{path}?{}", pairs.finish()));
                        }
                    }
                } else {
                    rewrite_live_id(value, id);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                rewrite_live_id(value, id);
            }
        }
        _ => {}
    }
}

pub async fn post_live_stream_action(
    State(state): State<AppState>,
    CatalogPreprocessed(preprocessed): CatalogPreprocessed,
) -> Result<Response, StatusCode> {
    let scope = preprocessed
        .access_scope
        .as_ref()
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let mut payload = body(&preprocessed.original_request)?;
    let id = field(&preprocessed.original_request, &payload, "LiveStreamId")?
        .ok_or(StatusCode::BAD_REQUEST)?;
    let binding = state
        .play_sessions
        .live_stream(&id, scope.user_id())
        .await
        .ok_or(StatusCode::BAD_REQUEST)?;
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
    let mut request = preprocessed
        .original_request
        .try_clone()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let close = request
        .url()
        .path()
        .trim_end_matches('/')
        .to_ascii_lowercase()
        .ends_with("/close");
    let pairs = request
        .url()
        .query_pairs()
        .filter(|(key, _)| !key.eq_ignore_ascii_case("LiveStreamId"))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    request
        .url_mut()
        .query_pairs_mut()
        .clear()
        .extend_pairs(pairs)
        .append_pair("LiveStreamId", &binding.original_id);
    let object = payload.as_object_mut().ok_or(StatusCode::BAD_REQUEST)?;
    object.retain(|key, _| !key.eq_ignore_ascii_case("LiveStreamId"));
    object.insert("LiveStreamId".into(), Value::String(binding.original_id));
    set_json_body(&mut request, &payload)?;
    let session = Some(session.clone());
    let auth = remap_authorization(&preprocessed.auth, &session)
        .await
        .map_err(|_| StatusCode::UNAUTHORIZED)?;
    apply_to_request(&mut request, server, &session, &auth, &state, Some(scope)).await;
    if close {
        let response = state
            .reqwest_client
            .execute(request)
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY)?;
        let status = response.status();
        if status.is_success() {
            state
                .play_sessions
                .remove_live_stream(&id, scope.user_id())
                .await;
        }
        return Ok(status.into_response());
    }
    let mut response = execute_processed_json_request(
        &state,
        request,
        server,
        ResponseProcessingProfile::Media,
        preprocessed.auth.as_ref().and_then(|auth| auth.token_ref()),
    )
    .await?;
    rewrite_live_id(&mut response, &id);
    Ok(Json(response).into_response())
}

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
    let mut payload = body(&preprocessed.original_request)?;
    let session_id = play_session_id(preprocessed.original_request.url())
        .map_err(|_| StatusCode::BAD_REQUEST)?
        .or(field(
            &preprocessed.original_request,
            &payload,
            "PlaySessionId",
        )?)
        .ok_or(StatusCode::BAD_REQUEST)?;
    let (binding, _) = state
        .play_sessions
        .resolve_report_session(&session_id, scope.user_id())
        .await
        .map_err(|_| StatusCode::FORBIDDEN)?
        .ok_or(StatusCode::BAD_REQUEST)?;
    let item_id = field(&preprocessed.original_request, &payload, "ItemId")?
        .ok_or(StatusCode::BAD_REQUEST)?;
    let item_id = crate::media_storage_service::MediaStorageService::normalize_uuid(&item_id);
    let item_id = &item_id;
    if !state
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
    let object = payload.as_object_mut().ok_or(StatusCode::BAD_REQUEST)?;
    object.retain(|key, _| {
        !["ItemId", "UserId", "PlaySessionId"]
            .iter()
            .any(|field| key.eq_ignore_ascii_case(field))
    });
    object.insert("ItemId".into(), Value::String(original_item_id.into()));
    object.insert(
        "UserId".into(),
        Value::String(session.as_ref().unwrap().original_user_id.clone()),
    );
    object.insert("PlaySessionId".into(), Value::String(session_id.clone()));
    set_json_body(&mut request, &payload)?;
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
    let mut source: MediaSource = serde_json::from_value(response["MediaSource"].clone())
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    bind_live_source(&state, &mut source, scope.user_id(), server).await?;
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
