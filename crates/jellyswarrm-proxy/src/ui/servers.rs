use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    Form,
};
use serde::Deserialize;
use tracing::{error, info};

use crate::{server_storage::Server, url_helper::join_server_url, AppState};

#[derive(Template)]
#[template(path = "servers.html")]
pub struct ServersPageTemplate {
    pub ui_route: String,
}

#[derive(Template)]
#[template(path = "server_list.html")]
pub struct ServerListTemplate {
    pub servers: Vec<Server>,
    pub ui_route: String,
}

#[derive(Template)]
#[template(path = "server_status.html")]
pub struct ServerStatusTemplate {
    pub error_message: Option<String>,
}

#[derive(Deserialize)]
pub struct AddServerForm {
    pub name: String,
    pub url: String,
    pub priority: i32,
}

#[derive(Deserialize)]
pub struct UpdatePriorityForm {
    pub priority: i32,
}

/// Main servers management page
pub async fn servers_page(State(state): State<AppState>) -> impl IntoResponse {
    let template = ServersPageTemplate {
        ui_route: state.get_ui_route().await,
    };

    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render servers template: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

/// Get server list partial (for HTMX)
pub async fn get_server_list(State(state): State<AppState>) -> impl IntoResponse {
    match state.server_storage.list_servers().await {
        Ok(servers) => {
            let template = ServerListTemplate {
                servers,
                ui_route: state.get_ui_route().await,
            };

            match template.render() {
                Ok(html) => Html(html).into_response(),
                Err(e) => {
                    error!("Failed to render server list template: {}", e);
                    (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
                }
            }
        }
        Err(e) => {
            error!("Failed to fetch servers: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response()
        }
    }
}

/// Add a new server
pub async fn add_server(
    State(state): State<AppState>,
    Form(form): Form<AddServerForm>,
) -> Response {
    // Validate the form data
    if form.name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Server name cannot be empty</div>"),
        )
            .into_response();
    }

    if form.url.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Server URL cannot be empty</div>"),
        )
            .into_response();
    }

    if form.priority < 1 || form.priority > 999 {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Priority must be between 1 and 999</div>"),
        )
            .into_response();
    }

    // Try to add the server
    match state
        .server_storage
        .add_server(form.name.trim(), form.url.trim(), form.priority)
        .await
    {
        Ok(server_id) => {
            info!(
                "Added new server: {} ({}) with ID: {}",
                form.name, form.url, server_id
            );

            // Return updated server list
            get_server_list(State(state)).await.into_response()
        }
        Err(e) => {
            error!("Failed to add server: {}", e);

            let error_message = if e.to_string().contains("UNIQUE constraint failed") {
                "A server with that name already exists"
            } else if e.to_string().contains("Invalid URL") {
                "Invalid URL format"
            } else {
                "Failed to add server"
            };

            (
                StatusCode::BAD_REQUEST,
                Html(format!(
                    "<div class=\"alert alert-error\">{error_message}</div>"
                )),
            )
                .into_response()
        }
    }
}

/// Delete a server
pub async fn delete_server(State(state): State<AppState>, Path(server_id): Path<i64>) -> Response {
    match state.server_storage.delete_server(server_id).await {
        Ok(true) => {
            info!("Deleted server with ID: {}", server_id);
            // Return updated server list
            get_server_list(State(state)).await.into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Html("<div class=\"alert alert-error\">Server not found</div>"),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to delete server: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to delete server</div>"),
            )
                .into_response()
        }
    }
}

/// Update server priority
pub async fn update_server_priority(
    State(state): State<AppState>,
    Path(server_id): Path<i64>,
    Form(form): Form<UpdatePriorityForm>,
) -> Response {
    if form.priority < 1 || form.priority > 999 {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Priority must be between 1 and 999</div>"),
        )
            .into_response();
    }

    match state
        .server_storage
        .update_server_priority(server_id, form.priority)
        .await
    {
        Ok(true) => {
            info!("Updated server {} priority to {}", server_id, form.priority);
            // Return updated server list
            get_server_list(State(state)).await.into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Html("<div class=\"alert alert-error\">Server not found</div>"),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to update server priority: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to update priority</div>"),
            )
                .into_response()
        }
    }
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
