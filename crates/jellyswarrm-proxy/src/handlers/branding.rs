use axum::{extract::State, Json};
use hyper::StatusCode;
use jellyfin_api::JellyfinClient;

use crate::{models::BrandingConfig, AppState};

pub async fn handle_branding(
    State(state): State<AppState>,
) -> Result<Json<BrandingConfig>, StatusCode> {
    let servers = state
        .server_storage
        .list_servers()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut message = "Jellyswarrm proxying to the following servers: ".to_string();
    let mut custom_css = String::new();

    if !servers.is_empty() {
        let server_links: Vec<String> = servers
            .iter()
            .map(|s| {
                format!(
                    "<a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">{}</a>",
                    s.url, s.name
                )
            })
            .collect();
        message.push_str(&server_links.join(", "));

        for server in servers {
            if state
                .server_storage
                .server_status(server.id)
                .await
                .is_healthy()
            {
                if let Ok(client) = JellyfinClient::new_with_client(
                    server.url.as_ref(),
                    state.server_storage.client_info.clone(),
                    state.server_storage.http_client.clone(),
                ) {
                    if let Ok(branding) = client.get_branding_configuration().await {
                        if let Some(remote_custom_css) = branding.custom_css {
                            custom_css = remote_custom_css;
                        }
                    }
                }
            }
        }
    } else {
        message.push_str("No servers configured.");
    }

    let config = BrandingConfig {
        login_disclaimer: message,
        custom_css,
        splashscreen_enabled: false,
    };
    Ok(Json(config))
}
