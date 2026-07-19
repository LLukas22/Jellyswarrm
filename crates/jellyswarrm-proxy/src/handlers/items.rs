use axum::{extract::State, Json};
use hyper::StatusCode;
use tracing::{debug, error, warn};

use crate::{
    extractors::{Preprocessed, RequireSession},
    handlers::common::{
        execute_json_request, execute_processed_json_request, payload_from_request,
        process_playback_response, remap_playback_request, set_json_body,
    },
    models::{PlaybackRequest, PlaybackResponse},
    processors::response_processor::ResponseProcessingProfile,
    request_preprocessing::PreprocessedRequest,
    virtual_library_service::VirtualLibraryResolution,
    AppState,
};

async fn get_processed_item_json(
    state: &AppState,
    preprocessed: PreprocessedRequest,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let virtual_library = preprocessed
        .original_request
        .url()
        .path_segments()
        .and_then(Iterator::last)
        .map(str::to_string);
    let server = preprocessed.server;
    let proxy_api_key = preprocessed
        .user
        .as_ref()
        .map(|user| user.virtual_key.clone());

    let mut response = execute_processed_json_request(
        state,
        preprocessed.request,
        &server,
        ResponseProcessingProfile::Media,
        false,
        proxy_api_key.as_deref(),
    )
    .await?;

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
            let name = resolved.library.name();
            response["Id"] = serde_json::Value::String(virtual_id.clone());
            response["DisplayPreferencesId"] = serde_json::Value::String(virtual_id);
            response["Name"] = serde_json::Value::String(name.to_string());
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
    get_processed_item_json(&state, preprocessed).await
}

//http://localhost:3000/Users/7bc57a386ab84999ad7262210a9cd253/Items?SortBy=SortName%2CProductionYear&SortOrder=Ascending&IncludeItemTypes=Movie&Recursive=true&Fields=PrimaryImageAspectRatio%2CMediaSourceCount&ImageTypeLimit=1&EnableImageTypes=Primary%2CBackdrop%2CBanner%2CThumb&StartIndex=0&ParentId=5f7e146c44d84b479cafecd3280be4ea&Limit=100
//http://localhost:3000/Items/430c368c5eb34534bf98363d5adbb92f/Similar?userId=520ea298ed8044338a28d912523d715f&limit=12&fields=PrimaryImageAspectRatio%2CCanDelete
pub async fn get_items(
    State(state): State<AppState>,
    Preprocessed(preprocessed): Preprocessed,
) -> Result<Json<serde_json::Value>, StatusCode> {
    get_processed_item_json(&state, preprocessed).await
}

// can be used for special features etc.
pub async fn get_items_list(
    State(state): State<AppState>,
    Preprocessed(preprocessed): Preprocessed,
) -> Result<Json<serde_json::Value>, StatusCode> {
    get_processed_item_json(&state, preprocessed).await
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
    let original_request = preprocessed.original_request;
    let payload: PlaybackRequest = payload_from_request(&original_request)?;

    if payload.device_profile.is_none() {
        warn!("Got playback request from client without device profile. Transcoding will be enforced!")
    }

    let server = preprocessed.server;

    let mut payload = payload;
    remap_playback_request(&mut payload, &state, &session).await?;

    debug!("Forwarding PlaybackRequest JSON: {:?}", &payload);

    let mut request = preprocessed.request;
    set_json_body(&mut request, &payload)?;

    match execute_json_request::<PlaybackResponse>(&state.reqwest_client, request).await {
        Ok(mut response) => {
            process_playback_response(&mut response, &state, &server, &session).await?;

            debug!("Requested Playback: {:?}", response);

            Ok(Json(response))
        }
        Err(e) => {
            error!("Failed to get playback info: {:?}", e);
            Err(e)
        }
    }
}
