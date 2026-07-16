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

fn local_server_info(state: &AppState, cfg: &crate::config::AppConfig) -> crate::models::ServerInfo {
    let _ = state;
    crate::models::ServerInfo {
        operating_system_display_name: Some(std::env::consts::OS.to_string()),
        has_pending_restart: Some(false),
        is_shutting_down: Some(false),
        supports_library_monitor: Some(false),
        web_socket_port_number: None,
        completed_installations: None,
        can_self_restart: Some(false),
        can_launch_web_browser: Some(false),
        program_data_path: None,
        web_path: None,
        items_by_name_path: None,
        cache_path: None,
        log_path: None,
        internal_metadata_path: None,
        transcoding_temp_path: None,
        cast_receiver_applications: None,
        has_update_available: Some(false),
        encoder_location: None,
        system_architecture: Some(std::env::consts::ARCH.to_string()),
        local_address: cfg.public_address.clone(),
        server_name: cfg.server_name.clone(),
        version: Some(reported_server_version()),
        operating_system: Some(std::env::consts::OS.to_string()),
        id: cfg.server_id.clone(),
        startup_wizard_completed: Some(true),
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
    // Prefer enriching from upstream, but never fail login/home on upstream 401.
    // Stale device-session remaps used to make this return 500 and stall the web UI.
    match execute_json_request::<crate::models::ServerInfo>(
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
            // Keep Version aligned with the embedded jellyfin-web client.
            server_info.version = Some(reported_server_version());

            Ok(Json(server_info))
        }
        Err(e) => {
            error!(
                "Upstream System/Info failed ({:?}); returning local proxy system info",
                e
            );
            let cfg = state.config.read().await;
            Ok(Json(local_server_info(&state, &cfg)))
        }
    }
}
