use axum::{
    extract::{Request, State},
    Json,
};
use hyper::StatusCode;
use reqwest::header::{HeaderValue, CONTENT_LENGTH, TRANSFER_ENCODING};
use reqwest::Body;
use tracing::{debug, error};

use crate::{
    handlers::common::{
        execute_json_request, payload_from_request, process_media_item, process_media_source,
        track_play_session,
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

    match execute_json_request::<MediaItem>(&state.reqwest_client, preprocessed.request).await {
        Ok(media_item) => {
            let server_id = { state.config.read().await.server_id.clone() };
            Ok(Json(
                process_media_item(media_item, &state, &server, false, &server_id).await?,
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

    match execute_json_request::<crate::models::ItemsResponseVariants>(
        &state.reqwest_client,
        preprocessed.request,
    )
    .await
    {
        Ok(mut response) => {
            let server_id = { state.config.read().await.server_id.clone() };
            for item in &mut response.iter_mut_items() {
                *item =
                    process_media_item(item.clone(), &state, &server, false, &server_id).await?;
            }

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

    match execute_json_request::<Vec<MediaItem>>(&state.reqwest_client, preprocessed.request).await
    {
        Ok(mut response) => {
            let server_id = { state.config.read().await.server_id.clone() };
            for item in &mut response {
                *item =
                    process_media_item(item.clone(), &state, &server, false, &server_id).await?;
            }

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

    let server = preprocessed.server;

    let session = preprocessed.session.ok_or(StatusCode::UNAUTHORIZED)?;

    let mut payload = payload;
    if payload.user_id.is_some() {
        payload.user_id = Some(session.original_user_id.clone());
    }

    if let Some(media_source_id) = &payload.media_source_id {
        if let Some(media_mapping) = state
            .media_storage
            .get_media_mapping_by_virtual(media_source_id)
            .await
            .unwrap_or_default()
        {
            payload.media_source_id = Some(media_mapping.original_media_id);
        }
    }

    debug!("Forwarding PlaybackRequest JSON: {:?}", &payload);

    let json = serde_json::to_vec(&payload).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Set body as a full buffer and provide Content-Length so upstream servers
    // don't wait for chunked data (which can trigger MinRequestBodyDataRate errors).
    let len = json.len();
    let mut request = preprocessed.request;
    *request.body_mut() = Some(Body::from(json));
    // Ensure Content-Length is set and remove Transfer-Encoding if present.
    request.headers_mut().insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&len.to_string()).unwrap(),
    );
    request.headers_mut().remove(TRANSFER_ENCODING);

    match execute_json_request::<PlaybackResponse>(&state.reqwest_client, request).await {
        Ok(mut response) => {
            for item in &mut response.media_sources {
                *item = process_media_source(item.clone(), &state.media_storage, &server).await?;
                track_play_session(item, &response.play_session_id, &server, &state).await?;
            }

            debug!("Requested Playback: {:?}", response);

            Ok(Json(response))
        }
        Err(e) => {
            error!("Failed to get playback info: {:?}", e);
            Err(e)
        }
    }
}
