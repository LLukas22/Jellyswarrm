//! Adapt streaming, HLS, and media resource HTTP requests to playback delivery workflows.
use axum::{extract::State, response::Response};
use hyper::StatusCode;

use crate::{
    application::playback::resources,
    http::{extractors::PlaybackInput, state::MediaState},
};

mod delivery;

pub(crate) async fn get_download(
    State(state): State<MediaState>,
    PlaybackInput(operation): PlaybackInput,
) -> Result<Response, StatusCode> {
    delivery::response(
        resources::get_download_typed(&state.playback_service(), operation)
            .await
            .map_err(|error| crate::http::handlers::pipeline_error::detail_status(&error))?,
    )
}

pub(crate) async fn get_video_resource(
    State(state): State<MediaState>,
    PlaybackInput(operation): PlaybackInput,
) -> Result<Response, StatusCode> {
    delivery::response(
        resources::get_video_resource_typed(&state.playback_service(), operation)
            .await
            .map_err(|error| crate::http::handlers::pipeline_error::detail_status(&error))?,
    )
}

pub(crate) async fn get_stream(
    State(state): State<MediaState>,
    PlaybackInput(operation): PlaybackInput,
) -> Result<Response, StatusCode> {
    delivery::response(
        resources::get_stream_typed(&state.playback_service(), operation)
            .await
            .map_err(|error| crate::http::handlers::pipeline_error::detail_status(&error))?,
    )
}
