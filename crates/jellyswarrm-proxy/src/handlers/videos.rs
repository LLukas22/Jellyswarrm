use axum::{extract::State, response::Response};
use hyper::StatusCode;

use crate::{application::playback::resources, extractors::PlaybackInput, AppState};

mod delivery;

pub async fn get_video_resource(
    State(state): State<AppState>,
    PlaybackInput(operation): PlaybackInput,
) -> Result<Response, StatusCode> {
    delivery::response(
        resources::get_video_resource_typed(&state.playback_service(), operation)
            .await
            .map_err(|error| error.detail_status())?,
    )
}

pub async fn get_stream(
    State(state): State<AppState>,
    PlaybackInput(operation): PlaybackInput,
) -> Result<Response, StatusCode> {
    delivery::response(
        resources::get_stream_typed(&state.playback_service(), operation)
            .await
            .map_err(|error| error.detail_status())?,
    )
}
