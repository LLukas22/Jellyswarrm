//! Adapt Jellyfin login and user requests to local account and federation workflows.
use axum::{
    extract::{Path, State},
    Json,
};
use hyper::{HeaderMap, StatusCode};

use crate::{
    application::accounts, http::extractors::OperationInput, models::AuthenticateRequest, AppState,
};

pub async fn handle_public(
    _state: State<AppState>,
) -> Result<Json<Vec<crate::models::User>>, StatusCode> {
    Ok(Json(accounts::public_users()))
}

pub async fn handle_get_me(
    State(state): State<AppState>,
    OperationInput(input): OperationInput,
) -> Result<Json<crate::models::User>, StatusCode> {
    accounts::get_me(&state, input)
        .await
        .map(Json)
        .map_err(|error| error.detail_status())
}

pub async fn handle_get_user_by_id(
    State(state): State<AppState>,
    Path(_user_id): Path<String>,
    OperationInput(input): OperationInput,
) -> Result<Json<crate::models::User>, StatusCode> {
    accounts::get_user_by_id(&state, input)
        .await
        .map(Json)
        .map_err(|error| error.detail_status())
}

pub async fn handle_authenticate_by_name(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<AuthenticateRequest>,
) -> Result<crate::sessions::AuthenticationResponse, StatusCode> {
    accounts::authenticate_by_name(&state, headers, payload)
        .await
        .map(crate::sessions::authentication_response)
        .map_err(|error| error.detail_status())
}
