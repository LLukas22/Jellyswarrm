use axum::{
    extract::State,
    response::{IntoResponse, Response},
    Json,
};
use hyper::StatusCode;
use serde_json::Value;

use crate::{
    application::playback::live::{self, LiveStreamActionResult},
    extractors::LiveInput,
    AppState,
};

pub async fn post_live_stream_action(
    State(state): State<AppState>,
    LiveInput(preprocessed): LiveInput,
) -> Result<Response, StatusCode> {
    let result = live::post_live_stream_action_typed(&state.playback_service(), preprocessed)
        .await
        .map_err(|error| error.detail_status())?;
    Ok(match result {
        LiveStreamActionResult::Status(status) => status.into_response(),
        LiveStreamActionResult::Json(payload) => Json(payload).into_response(),
    })
}

pub async fn post_livestream_open(
    State(state): State<AppState>,
    LiveInput(preprocessed): LiveInput,
) -> Result<Json<Value>, StatusCode> {
    live::post_livestream_open_typed(&state.playback_service(), preprocessed)
        .await
        .map(Json)
        .map_err(|error| error.detail_status())
}
