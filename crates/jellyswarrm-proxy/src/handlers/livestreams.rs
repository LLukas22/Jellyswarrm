use axum::{extract::State, Json};
use hyper::StatusCode;
use tracing::{debug, error};

use crate::{
    extractors::RequireSession,
    handlers::common::{
        execute_json_request, payload_from_request, process_playback_response,
        remap_playback_request, set_json_body,
    },
    models::{PlaybackRequest, PlaybackResponse},
    AppState,
};

//http://localhost:3000/LiveStreams/Open?UserId=b88ec8ff27774f26a992ce60e3190b46&StartTimeTicks=0&ItemId=31204dde7d38420f8b166d02b26f8c75&PlaySessionId=b33ff036839b4e0992fb374ddcd24e7d&MaxStreamingBitrate=2147483647
#[axum::debug_handler]
#[allow(dead_code)]
pub async fn post_livestream_open(
    State(state): State<AppState>,
    RequireSession {
        preprocessed,
        session,
    }: RequireSession,
) -> Result<Json<PlaybackResponse>, StatusCode> {
    let original_request = preprocessed.original_request;
    let payload: PlaybackRequest = payload_from_request(&original_request)?;

    let server = preprocessed.server;

    let mut payload = payload;
    remap_playback_request(&mut payload, &state, &session).await?;

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
