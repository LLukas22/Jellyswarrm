//! Serve proxy system information and backend-compatible discovery responses.
use crate::{
    application::accounts::{
        self,
        services::{AccountService, AuthenticationService},
    },
    http::{assets::version::JELLYFIN_UI_VERSION, extractors::OperationInput},
};
use axum::{extract::State, Json};
use hyper::StatusCode;

pub(crate) async fn info_public(
    State(state): State<AuthenticationService>,
) -> Result<Json<crate::models::PublicServerInfo>, StatusCode> {
    let ui_version = JELLYFIN_UI_VERSION.clone().unwrap_or_default().version;
    Ok(Json(
        accounts::public_server_info(&state, &ui_version).await,
    ))
}

pub(crate) async fn info(
    State(state): State<AccountService>,
    OperationInput(input): OperationInput,
) -> Result<Json<crate::models::ServerInfo>, StatusCode> {
    let ui_version = JELLYFIN_UI_VERSION.clone().unwrap_or_default().version;
    accounts::server_info(&state, input, &ui_version)
        .await
        .map(Json)
        .map_err(|error| crate::http::handlers::pipeline_error::detail_status(&error))
}
