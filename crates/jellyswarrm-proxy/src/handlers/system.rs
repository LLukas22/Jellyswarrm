use axum::{extract::State, Json};
use hyper::StatusCode;
use tracing::error;

use crate::{
    extractors::RequireUser, handlers::common::execute_json_request, ui::JELLYFIN_UI_VERSION,
    AppState,
};

fn reported_server_version() -> String {
    // Jellyfin Web refuses to load when Version is empty/unknown ("Update Required").
    // Always report the embedded web client's version so the UI stays compatible.
    let version = JELLYFIN_UI_VERSION
        .clone()
        .unwrap_or_default()
        .version
        .trim()
        .to_string();
    if version.is_empty() || version.eq_ignore_ascii_case("unknown") {
        // Fallback for test builds that skip ui-version.env generation.
        "10.11.0".to_string()
    } else {
        version
    }
}

pub async fn info_public(
    State(state): State<AppState>,
) -> Result<Json<crate::models::PublicServerInfo>, StatusCode> {
    let cfg = state.config.read().await;

    Ok(Json(crate::models::PublicServerInfo {
        id: cfg.server_id.clone(),
        server_name: cfg.server_name.clone(),
        local_address: cfg.public_address.clone(),
        version: reported_server_version(),
        product_name: "Jellyfin Server".to_string(),
        operating_system: std::env::consts::OS.to_string(),
        startup_wizard_completed: true,
    }))
}

pub async fn info(
    State(state): State<AppState>,
    RequireUser { preprocessed, .. }: RequireUser,
) -> Result<Json<crate::models::ServerInfo>, StatusCode> {
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
            // Keep Version aligned with the embedded jellyfin-web client. Leaving the
            // upstream version can surface "Update Required" when backends differ.
            server_info.version = Some(reported_server_version());

            Ok(Json(server_info))
        }
        Err(e) => {
            error!("Failed to get server info: {:?}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}
