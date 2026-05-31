use axum::{
    extract::{Request, State},
    Json,
};
use hyper::StatusCode;
use tracing::{debug, error, warn};

use crate::{
    handlers::common::{
        execute_json_request, payload_from_request, process_items_response, process_media_item,
        process_media_items, process_playback_response, remap_playback_request, set_json_body,
    },
    models::{MediaItem, PlaybackRequest, PlaybackResponse},
    request_preprocessing::preprocess_request,
    AppState,
};

//http://localhost:3000/Users/7bc57a386ab84999ad7262210a9cd253/Items/5f7e146c44d84b479cafecd3280be4ea
//http://localhost:3000/Items/430c368c5eb34534bf98363d5adbb92f?userId=520ea298ed8044338a28d912523d715f
pub async fn get_item(
    State(state): State<AppState>,
    req: Request,
) -> Result<Json<MediaItem>, StatusCode> {
    let preprocessed = preprocess_request(req, &state).await.map_err(|e| {
        error!("Failed to preprocess request: {}", e);
        StatusCode::BAD_REQUEST
    })?;

    let server = preprocessed.server;

    let proxy_api_key = preprocessed
        .user
        .as_ref()
        .map(|user| user.virtual_key.clone());

    match execute_json_request::<MediaItem>(&state.reqwest_client, preprocessed.request).await {
        Ok(media_item) => {
            let server_id = { state.config.read().await.server_id.clone() };
            Ok(Json(
                process_media_item(
                    media_item,
                    &state,
                    &server,
                    false,
                    &server_id,
                    proxy_api_key.as_deref(),
                )
                .await?,
            ))
        }
        Err(e) => {
            error!("Failed to get MediaItem: {:?}", e);
            Err(e)
        }
    }
}

//http://localhost:3000/Users/7bc57a386ab84999ad7262210a9cd253/Items?SortBy=SortName%2CProductionYear&SortOrder=Ascending&IncludeItemTypes=Movie&Recursive=true&Fields=PrimaryImageAspectRatio%2CMediaSourceCount&ImageTypeLimit=1&EnableImageTypes=Primary%2CBackdrop%2CBanner%2CThumb&StartIndex=0&ParentId=5f7e146c44d84b479cafecd3280be4ea&Limit=100
//http://localhost:3000/Items/430c368c5eb34534bf98363d5adbb92f/Similar?userId=520ea298ed8044338a28d912523d715f&limit=12&fields=PrimaryImageAspectRatio%2CCanDelete
pub async fn get_items(
    State(state): State<AppState>,
    req: Request,
) -> Result<Json<crate::models::ItemsResponseVariants>, StatusCode> {
    let preprocessed = preprocess_request(req, &state).await.map_err(|e| {
        error!("Failed to preprocess request: {}", e);
        StatusCode::BAD_REQUEST
    })?;

    let server = preprocessed.server;

    let proxy_api_key = preprocessed
        .user
        .as_ref()
        .map(|user| user.virtual_key.clone());

    match execute_json_request::<crate::models::ItemsResponseVariants>(
        &state.reqwest_client,
        preprocessed.request,
    )
    .await
    {
        Ok(mut response) => {
            let server_id = { state.config.read().await.server_id.clone() };
            process_items_response(
                &mut response,
                &state,
                &server,
                false,
                &server_id,
                proxy_api_key.as_deref(),
            )
            .await?;

            Ok(Json(response))
        }
        Err(e) => {
            error!("Failed to get ItemsResponse: {:?}", e);
            Err(e)
        }
    }
}

// can be used for special features etc.
pub async fn get_items_list(
    State(state): State<AppState>,
    req: Request,
) -> Result<Json<Vec<MediaItem>>, StatusCode> {
    let preprocessed = preprocess_request(req, &state).await.map_err(|e| {
        error!("Failed to preprocess request: {}", e);
        StatusCode::BAD_REQUEST
    })?;

    let server = preprocessed.server;

    let proxy_api_key = preprocessed
        .user
        .as_ref()
        .map(|user| user.virtual_key.clone());

    match execute_json_request::<Vec<MediaItem>>(&state.reqwest_client, preprocessed.request).await
    {
        Ok(mut response) => {
            let server_id = { state.config.read().await.server_id.clone() };
            process_media_items(
                &mut response,
                &state,
                &server,
                false,
                &server_id,
                proxy_api_key.as_deref(),
            )
            .await?;

            Ok(Json(response))
        }
        Err(e) => {
            error!("Failed to get Vec<MediaItem>: {:?}", e);
            Err(e)
        }
    }
}

//http://192.168.188.142:30013/Items/165a66aa5bd2e62c0df0f8da332ae47d/PlaybackInfo
#[axum::debug_handler]
pub async fn post_playback_info(
    State(state): State<AppState>,
    req: Request,
) -> Result<Json<PlaybackResponse>, StatusCode> {
    let preprocessed = preprocess_request(req, &state).await.map_err(|e| {
        error!("Failed to preprocess request: {}", e);
        StatusCode::BAD_REQUEST
    })?;

    let original_request = preprocessed
        .original_request
        .ok_or(StatusCode::BAD_REQUEST)?;
    let payload: PlaybackRequest = payload_from_request(&original_request)?;

    if payload.device_profile.is_none() {
        warn!("Got playback request from client without device profile. Transcoding will be enforced!")
    }

    let server = preprocessed.server;

    let session = preprocessed.session.ok_or(StatusCode::UNAUTHORIZED)?;

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
