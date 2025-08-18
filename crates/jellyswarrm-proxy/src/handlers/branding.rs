use axum::{extract::State, Json};
use hyper::StatusCode;

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
    } else {
        message.push_str("No servers configured.");
    }

    let config = BrandingConfig {
        login_disclaimer: message,
        custom_css: String::new(),
        splashscreen_enabled: false,
    };
    Ok(Json(config))
}
