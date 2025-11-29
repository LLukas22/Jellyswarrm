use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse},
};
use tracing::error;

use crate::{url_helper::join_server_url, AppState};

#[derive(Template)]
#[template(path = "admin/server_status.html")]
pub struct ServerStatusTemplate {
    pub error_message: Option<String>,
}

/// Check server status
pub async fn check_server_status(
    State(state): State<AppState>,
    Path(server_id): Path<i64>,
) -> impl IntoResponse {
    // Get the server details first
    match state.server_storage.get_server_by_id(server_id).await {
        Ok(Some(server)) => {
            // Test connection to the server
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap();

            let status_url = join_server_url(&server.url, "/system/info/public");

            match client.get(status_url.as_str()).send().await {
                Ok(response) if response.status().is_success() => {
                    let template = ServerStatusTemplate {
                        error_message: None,
                    };

                    match template.render() {
                        Ok(html) => Html(html).into_response(),
                        Err(e) => {
                            error!("Failed to render status template: {}", e);
                            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
                        }
                    }
                }
                Ok(response) => {
                    let template = ServerStatusTemplate {
                        error_message: Some(format!("HTTP {}", response.status().as_u16())),
                    };

                    match template.render() {
                        Ok(html) => Html(html).into_response(),
                        Err(e) => {
                            error!("Failed to render status template: {}", e);
                            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
                        }
                    }
                }
                Err(e) => {
                    let error_msg = if e.is_timeout() {
                        "Connection timeout".to_string()
                    } else if e.is_connect() {
                        "Connection refused".to_string()
                    } else {
                        format!("Network error: {e}")
                    };

                    let template = ServerStatusTemplate {
                        error_message: Some(error_msg),
                    };

                    match template.render() {
                        Ok(html) => Html(html).into_response(),
                        Err(e) => {
                            error!("Failed to render status template: {}", e);
                            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
                        }
                    }
                }
            }
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Html("<span style=\"color: #dc3545;\">Server not found</span>"),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to get server: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<span style=\"color: #dc3545;\">Database error</span>"),
            )
                .into_response()
        }
    }
}
