use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse},
};
use tracing::error;

use crate::{server_storage::ServerHealthStatus, AppState};

#[derive(Template)]
#[template(path = "admin/server_status.html")]
pub struct ServerStatusTemplate {
    pub error_message: Option<String>,
    pub server_version: Option<String>,
}

/// Check server status
pub async fn check_server_status(
    State(state): State<AppState>,
    Path(server_id): Path<i64>,
) -> impl IntoResponse {
    // Get the server details first
    match state.server_storage.get_server_by_id(server_id).await {
        Ok(Some(_)) => match state.server_storage.server_status(server_id).await {
            ServerHealthStatus::Healthy(info) => {
                let template = ServerStatusTemplate {
                    error_message: None,
                    server_version: info.version,
                };

                match template.render() {
                    Ok(html) => Html(html).into_response(),
                    Err(e) => {
                        error!("Failed to render status template: {}", e);
                        (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
                    }
                }
            }
            ServerHealthStatus::Unhealthy(e) => {
                let template = ServerStatusTemplate {
                    error_message: Some(format!("Error: {}", e)),
                    server_version: None,
                };

                match template.render() {
                    Ok(html) => Html(html).into_response(),
                    Err(e) => {
                        error!("Failed to render status template: {}", e);
                        (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
                    }
                }
            }
        },
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
