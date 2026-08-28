use axum::{extract::State, Json};
use hyper::StatusCode;
use tracing::{debug, error, warn};

use crate::{
    extractors::{Preprocessed, RequireSession},
    handlers::common::{
        execute_json_request, execute_processed_json_request, payload_from_request,
        process_playback_response, remap_playback_request, set_json_body, track_playback_alias,
    },
    handlers::media_dedup::{
        merge_movie_detail, record_playback_sources, resolve_playback_route, DetailMergeContext,
        PlaybackRouteDecision,
    },
    models::{PlaybackRequest, PlaybackResponse},
    processors::response_processor::ResponseProcessingProfile,
    request_preprocessing::PreprocessedRequest,
    url_helper::{contains_id, ensure_query_list_value},
    virtual_library_service::VirtualLibraryResolution,
    AppState,
};

async fn get_processed_item_json(
    state: &AppState,
    mut preprocessed: PreprocessedRequest,
    merge_movie_versions: bool,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let virtual_library = preprocessed
        .original_request
        .url()
        .path_segments()
        .and_then(Iterator::last)
        .map(str::to_string);
    let requested_item_id = contains_id(preprocessed.original_request.url(), "Items");
    let server = preprocessed.server.clone();
    let source_generation = if merge_movie_versions {
        Some(
            state
                .media_storage
                .begin_movie_reconciliation()
                .await
                .map_err(|error| {
                    error!("Failed to begin detail source reconciliation: {error}");
                    StatusCode::INTERNAL_SERVER_ERROR
                })?,
        )
    } else {
        None
    };
    let proxy_api_key = preprocessed
        .auth
        .as_ref()
        .and_then(|auth| auth.token_ref())
        .map(str::to_string);

    if merge_movie_versions {
        ensure_query_list_value(preprocessed.request.url_mut(), "Fields", "MediaSources");
    }

    let mut response = execute_processed_json_request(
        state,
        preprocessed.request,
        &server,
        ResponseProcessingProfile::Media,
        false,
        proxy_api_key.as_deref(),
    )
    .await?;

    if let (true, Some(requested_item_id)) = (merge_movie_versions, requested_item_id.as_deref()) {
        merge_movie_detail(
            state,
            DetailMergeContext {
                requested_item_id,
                base_server: &server,
                auth: &preprocessed.auth,
                access_scope: preprocessed.access_scope.as_ref(),
                sessions: preprocessed.sessions.as_deref(),
                original_request: &preprocessed.original_request,
                source_generation: source_generation
                    .expect("merged detail has a source generation"),
            },
            proxy_api_key.as_deref(),
            &mut response,
        )
        .await?;
    }

    if let Some(virtual_id) = virtual_library {
        let resolution = state
            .virtual_library_service
            .resolve(&virtual_id, preprocessed.access_scope.as_ref())
            .await
            .map_err(|error| {
                error!("Failed to resolve virtual library item: {error}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
        if let VirtualLibraryResolution::Resolved(resolved) = resolution {
            let name = resolved.name;
            response["Id"] = serde_json::Value::String(virtual_id.clone());
            response["DisplayPreferencesId"] = serde_json::Value::String(virtual_id);
            response["Name"] = serde_json::Value::String(name.clone());
            response["SortName"] = serde_json::Value::String(name.to_lowercase());
        }
    }

    Ok(Json(response))
}

//http://localhost:3000/Users/7bc57a386ab84999ad7262210a9cd253/Items/5f7e146c44d84b479cafecd3280be4ea
//http://localhost:3000/Items/430c368c5eb34534bf98363d5adbb92f?userId=520ea298ed8044338a28d912523d715f
pub async fn get_item(
    State(state): State<AppState>,
    Preprocessed(preprocessed): Preprocessed,
) -> Result<Json<serde_json::Value>, StatusCode> {
    get_processed_item_json(&state, preprocessed, true).await
}

//http://localhost:3000/Users/7bc57a386ab84999ad7262210a9cd253/Items?SortBy=SortName%2CProductionYear&SortOrder=Ascending&IncludeItemTypes=Movie&Recursive=true&Fields=PrimaryImageAspectRatio%2CMediaSourceCount&ImageTypeLimit=1&EnableImageTypes=Primary%2CBackdrop%2CBanner%2CThumb&StartIndex=0&ParentId=5f7e146c44d84b479cafecd3280be4ea&Limit=100
//http://localhost:3000/Items/430c368c5eb34534bf98363d5adbb92f/Similar?userId=520ea298ed8044338a28d912523d715f&limit=12&fields=PrimaryImageAspectRatio%2CCanDelete
pub async fn get_items(
    State(state): State<AppState>,
    Preprocessed(preprocessed): Preprocessed,
) -> Result<Json<serde_json::Value>, StatusCode> {
    get_processed_item_json(&state, preprocessed, false).await
}

// can be used for special features etc.
pub async fn get_items_list(
    State(state): State<AppState>,
    Preprocessed(preprocessed): Preprocessed,
) -> Result<Json<serde_json::Value>, StatusCode> {
    get_processed_item_json(&state, preprocessed, false).await
}

//http://192.168.188.142:30013/Items/165a66aa5bd2e62c0df0f8da332ae47d/PlaybackInfo
#[axum::debug_handler]
pub async fn post_playback_info(
    State(state): State<AppState>,
    RequireSession {
        preprocessed,
        session,
    }: RequireSession,
) -> Result<Json<PlaybackResponse>, StatusCode> {
    let proxy_api_key = preprocessed
        .auth
        .as_ref()
        .and_then(|auth| auth.token_ref())
        .map(str::to_string);
    let payload: PlaybackRequest = payload_from_request(&preprocessed.original_request)?;
    let requested_item_id =
        contains_id(preprocessed.original_request.url(), "Items").ok_or(StatusCode::BAD_REQUEST)?;
    let source_generation = state
        .media_storage
        .begin_movie_reconciliation()
        .await
        .map_err(|error| {
            error!("Failed to begin playback source reconciliation: {error}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    if payload.device_profile.is_none() {
        warn!("Got playback request from client without device profile. Transcoding will be enforced!")
    }

    let mut payload = payload;

    let route = resolve_playback_route(
        &state,
        &preprocessed,
        &requested_item_id,
        payload.media_source_id.as_deref(),
    )
    .await?;
    let (server, session, mut request) = match route {
        PlaybackRouteDecision::Original => (preprocessed.server, session, preprocessed.request),
        PlaybackRouteDecision::Rerouted(route) => (route.server, route.session, route.request),
        PlaybackRouteDecision::InvalidSelectedSource => return Err(StatusCode::BAD_REQUEST),
        PlaybackRouteDecision::SelectedSourceUnavailable => {
            return Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    };

    remap_playback_request(&mut payload, &state, &session).await?;

    debug!("Forwarding PlaybackRequest JSON: {:?}", &payload);

    set_json_body(&mut request, &payload)?;

    match execute_json_request::<PlaybackResponse>(&state.reqwest_client, request).await {
        Ok(mut response) => {
            process_playback_response(
                &mut response,
                &state,
                &server,
                &session,
                proxy_api_key.as_deref(),
            )
            .await?;
            record_playback_sources(
                &state,
                &requested_item_id,
                &server,
                source_generation,
                &mut response.media_sources,
            )
            .await?;
            track_playback_alias(
                &requested_item_id,
                &response.play_session_id,
                &session.user_id,
                &server,
                &state,
            )
            .await;

            debug!("Requested Playback: {:?}", response);

            Ok(Json(response))
        }
        Err(e) => {
            error!("Failed to get playback info: {:?}", e);
            Err(e)
        }
    }
}
