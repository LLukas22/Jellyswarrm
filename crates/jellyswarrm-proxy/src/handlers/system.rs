use axum::{
    extract::{Request, State},
    Json,
};
use hyper::StatusCode;
use tracing::error;

use crate::{
    handlers::common::execute_json_request, request_preprocessing::preprocess_request, AppState,
};

pub async fn info_public(
    State(state): State<AppState>,
    req: Request,
) -> Result<Json<crate::models::PublicServerInfo>, StatusCode> {
    let preprocessed = preprocess_request(req, &state).await.map_err(|e| {
        error!("Failed to preprocess request: {}", e);
        StatusCode::BAD_REQUEST
    })?;

    match execute_json_request::<crate::models::PublicServerInfo>(
        &state.reqwest_client,
        preprocessed.request,
    )
    .await
    {
        Ok(mut server_info) => {
            let cfg = state.config.read().await;
            server_info.id = cfg.server_id.clone();
            server_info.server_name = cfg.server_name.clone();
            server_info.local_address = cfg.public_address.clone();

            Ok(Json(server_info))
        }
        Err(e) => {
            error!("Failed to get server info: {:?}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

pub async fn info(
    State(state): State<AppState>,
    req: Request,
) -> Result<Json<crate::models::ServerInfo>, StatusCode> {
    let preprocessed = preprocess_request(req, &state).await.map_err(|e| {
        error!("Failed to preprocess request: {}", e);
        StatusCode::BAD_REQUEST
    })?;

    preprocessed.user.ok_or_else(|| {
        error!("User not found in request preprocessing");
        StatusCode::UNAUTHORIZED
    })?;

    // return Err(StatusCode::UNAUTHORIZED);

    match execute_json_request::<crate::models::ServerInfo>(
        &state.reqwest_client,
        preprocessed.request,
    )
    .await
    {
        Ok(mut server_info) => {
            let cfg = state.config.read().await;
            server_info.id = cfg.server_id.clone();
            server_info.server_name = "Jellyswarrm Proxy".to_string();
            server_info.local_address = cfg.public_address.clone();

            Ok(Json(server_info))
        }
        Err(e) => {
            error!("Failed to get server info: {:?}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}
