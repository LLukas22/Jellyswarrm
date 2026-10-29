//! Adapt live-stream open, refresh, and close HTTP requests to playback workflows.
use axum::{
    extract::State,
    response::{IntoResponse, Response},
    Json,
};
use hyper::StatusCode;
use serde_json::Value;

use crate::{
    application::playback::live::{self, LiveStreamActionResult},
    http::extractors::LiveInput,
    AppState,
};

pub(crate) async fn post_live_stream_action(
    State(state): State<AppState>,
    LiveInput(preprocessed): LiveInput,
) -> Result<Response, StatusCode> {
    let result = live::post_live_stream_action_typed(&state.playback_service(), preprocessed)
        .await
        .map_err(|error| crate::http::handlers::pipeline_error::detail_status(&error))?;
    Ok(match result {
        LiveStreamActionResult::Status(status) => status.into_response(),
        LiveStreamActionResult::Json(payload) => Json(payload).into_response(),
    })
}

pub(crate) async fn post_livestream_open(
    State(state): State<AppState>,
    LiveInput(preprocessed): LiveInput,
) -> Result<Json<Value>, StatusCode> {
    live::post_livestream_open_typed(&state.playback_service(), preprocessed)
        .await
        .map(Json)
        .map_err(|error| crate::http::handlers::pipeline_error::detail_status(&error))
}
