//! Adapt Jellyfin login and user requests to local account and federation workflows.
use axum::{
    extract::{Path, State},
    Json,
};
use hyper::{HeaderMap, StatusCode};

use crate::{
    application::accounts::{
        self,
        services::{AccountService, AuthenticationService},
    },
    http::extractors::OperationInput,
    models::AuthenticateRequest,
};

pub(crate) async fn handle_public() -> Result<Json<Vec<crate::models::User>>, StatusCode> {
    Ok(Json(accounts::public_users()))
}

pub(crate) async fn handle_get_me(
    State(state): State<AccountService>,
    OperationInput(input): OperationInput,
) -> Result<Json<crate::models::User>, StatusCode> {
    accounts::get_me(&state, input)
        .await
        .map(Json)
        .map_err(|error| crate::http::handlers::pipeline_error::detail_status(&error))
}

pub(crate) async fn handle_get_user_by_id(
    State(state): State<AccountService>,
    Path(_user_id): Path<String>,
    OperationInput(input): OperationInput,
) -> Result<Json<crate::models::User>, StatusCode> {
    accounts::get_user_by_id(&state, input)
        .await
        .map(Json)
        .map_err(|error| crate::http::handlers::pipeline_error::detail_status(&error))
}

pub(crate) async fn handle_authenticate_by_name(
    State(state): State<AuthenticationService>,
    headers: HeaderMap,
    Json(payload): Json<AuthenticateRequest>,
) -> Result<crate::http::sessions::AuthenticationResponse, StatusCode> {
    accounts::authenticate_by_name(&state, headers, payload)
        .await
        .map(crate::http::sessions::authentication_response)
        .map_err(|error| crate::http::handlers::pipeline_error::detail_status(&error))
}
