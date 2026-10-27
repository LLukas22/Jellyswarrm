//! Serve proxy system information and backend-compatible discovery responses.
use crate::{
    application::accounts, http::extractors::OperationInput, ui::JELLYFIN_UI_VERSION, AppState,
};
use axum::{extract::State, Json};
use hyper::StatusCode;

pub async fn info_public(
    State(state): State<AppState>,
) -> Result<Json<crate::models::PublicServerInfo>, StatusCode> {
    let ui_version = JELLYFIN_UI_VERSION.clone().unwrap_or_default().version;
    Ok(Json(
        accounts::public_server_info(&state.authentication_service().await, &ui_version).await,
    ))
}

pub async fn info(
    State(state): State<AppState>,
    OperationInput(input): OperationInput,
) -> Result<Json<crate::models::ServerInfo>, StatusCode> {
    let ui_version = JELLYFIN_UI_VERSION.clone().unwrap_or_default().version;
    accounts::server_info(&state.account_service().await, input, &ui_version)
        .await
        .map(Json)
        .map_err(|error| crate::http::handlers::pipeline_error::detail_status(&error))
}
