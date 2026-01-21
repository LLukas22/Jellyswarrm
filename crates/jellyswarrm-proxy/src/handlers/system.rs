use axum::{
    extract::{Request, State},
    Json,
};
use hyper::StatusCode;
use tracing::error;

use crate::{
    handlers::common::execute_json_request, request_preprocessing::preprocess_request,
    ui::JELLYFIN_UI_VERSION, AppState,
};

pub async fn info_public(
    State(state): State<AppState>,
) -> Result<Json<crate::models::PublicServerInfo>, StatusCode> {
    let cfg = state.config.read().await;

    Ok(Json(crate::models::PublicServerInfo {
        id: cfg.server_id.clone(),
        server_name: cfg.server_name.clone(),
        local_address: cfg.public_address.clone(),
        version: JELLYFIN_UI_VERSION.clone().unwrap_or_default().version,
        product_name: "Jellyfin Server".to_string(),
        operating_system: std::env::consts::OS.to_string(),
        startup_wizard_completed: true,
    }))
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
