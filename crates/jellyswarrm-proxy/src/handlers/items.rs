use axum::{extract::State, Json};
use hyper::StatusCode;

use crate::{
    application::{details, playback::negotiation as playback},
    extractors::{OperationInput, PlaybackInput},
    models::PlaybackResponse,
    AppState,
};

#[cfg(test)]
mod tests;

pub async fn get_item(
    State(state): State<AppState>,
    OperationInput(preprocessed): OperationInput,
) -> Result<Json<serde_json::Value>, StatusCode> {
    details::get_item_detail(&state.catalog_service(), preprocessed, true)
        .await
        .map(Json)
        .map_err(|error| error.detail_status())
}

pub async fn get_items(
    State(state): State<AppState>,
    OperationInput(preprocessed): OperationInput,
) -> Result<Json<serde_json::Value>, StatusCode> {
    details::get_item_detail(&state.catalog_service(), preprocessed, false)
        .await
        .map(Json)
        .map_err(|error| error.detail_status())
}

pub async fn get_items_list(
    State(state): State<AppState>,
    OperationInput(preprocessed): OperationInput,
) -> Result<Json<serde_json::Value>, StatusCode> {
    details::get_item_detail(&state.catalog_service(), preprocessed, false)
        .await
        .map(Json)
        .map_err(|error| error.detail_status())
}

#[axum::debug_handler]
pub async fn post_playback_info(
    State(state): State<AppState>,
    PlaybackInput(input): PlaybackInput,
) -> Result<Json<PlaybackResponse>, StatusCode> {
    playback::post_playback_info_typed(&state.playback_service(), input)
        .await
        .map(Json)
        .map_err(|error| error.detail_status())
}
