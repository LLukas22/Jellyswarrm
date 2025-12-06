use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    Form,
};
use serde::Deserialize;
use tracing::{error, info};

use crate::{
    encryption::{encrypt_password, Password},
    server_storage::Server,
    AppState,
};

#[derive(Template)]
#[template(path = "admin/servers.html")]
pub struct ServersPageTemplate {
    pub ui_route: String,
}

pub struct ServerWithAdmin {
    pub server: Server,
    pub has_admin: bool,
}

#[derive(Template)]
#[template(path = "admin/server_list.html")]
pub struct ServerListTemplate {
    pub servers: Vec<ServerWithAdmin>,
    pub ui_route: String,
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

#[derive(Deserialize)]
pub struct AddServerAdminForm {
    pub username: String,
    pub password: Password,
}

async fn render_server_list(state: &AppState) -> Result<String, String> {
    match state.server_storage.list_servers().await {
        Ok(servers) => {
            let mut servers_with_admin = Vec::new();
            for server in servers {
                let has_admin = state
                    .server_storage
                    .get_server_admin(server.id)
                    .await
                    .unwrap_or(None)
                    .is_some();
                servers_with_admin.push(ServerWithAdmin { server, has_admin });
            }

            let template = ServerListTemplate {
                servers: servers_with_admin,
                ui_route: state.get_ui_route().await,
            };

            template.render().map_err(|e| e.to_string())
        }
        Err(e) => Err(e.to_string()),
    }
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
    match render_server_list(&state).await {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render server list: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Error").into_response()
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

/// Add server admin
pub async fn add_server_admin(
    State(state): State<AppState>,
    Path(server_id): Path<i64>,
    Form(form): Form<AddServerAdminForm>,
) -> Response {
    // 1. Get server details
    let server = match state.server_storage.get_server_by_id(server_id).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Html("<div style=\"background-color: #e74c3c; color: white; padding: 0.75rem; border-radius: 0.25rem; margin-bottom: 1rem;\">Server not found</div>"),
            )
                .into_response()
        }
        Err(e) => {
            error!("Failed to get server: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div style=\"background-color: #e74c3c; color: white; padding: 0.75rem; border-radius: 0.25rem; margin-bottom: 1rem;\">Database error</div>"),
            )
                .into_response();
        }
    };

    // 2. Verify credentials with upstream Jellyfin and check admin status
    let client_info = crate::config::CLIENT_INFO.clone();

    let client = match jellyfin_api::JellyfinClient::new(server.url.as_str(), client_info) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to create jellyfin client: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div style=\"background-color: #e74c3c; color: white; padding: 0.75rem; border-radius: 0.25rem; margin-bottom: 1rem;\">Client error</div>"),
            )
                .into_response();
        }
    };

    match client
        .authenticate_by_name(&form.username, form.password.as_str())
        .await
    {
        Ok(user) => {
            // Check if user is admin
            let is_admin = user.policy.map(|p| p.is_administrator).unwrap_or(false);

            if !is_admin {
                return (
                    StatusCode::OK,
                    Html("<div style=\"background-color: #e74c3c; color: white; padding: 0.75rem; border-radius: 0.25rem; margin-bottom: 1rem;\">User is not an administrator on this server</div>"),
                )
                    .into_response();
            }

            // 3. Encrypt password with admin master password
            let config = state.config.read().await;
            let encrypted_password = match encrypt_password(&form.password, &config.password.clone().into()) {
                Ok(p) => p,
                Err(e) => {
                    error!("Encryption failed: {}", e);
                    return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Html("<div style=\"background-color: #e74c3c; color: white; padding: 0.75rem; border-radius: 0.25rem; margin-bottom: 1rem;\">Encryption failed</div>"),
                        )
                            .into_response();
                }
            };

            // 4. Save to database
            match state
                .server_storage
                .add_server_admin(server_id, &form.username, &encrypted_password)
                .await
            {
                Ok(_) => {
                    info!("Added admin for server {}", server.name);
                    match render_server_list(&state).await {
                        Ok(html) => Html(format!(
                            r#"<div id="server-list" hx-swap-oob="innerHTML">{}</div>"#,
                            html
                        ))
                        .into_response(),
                        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
                    }
                }
                Err(e) => {
                    error!("Failed to add server admin: {}", e);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Html("<div style=\"background-color: #e74c3c; color: white; padding: 0.75rem; border-radius: 0.25rem; margin-bottom: 1rem;\">Database error</div>"),
                    )
                        .into_response()
                }
            }
        }
        Err(jellyfin_api::error::Error::AuthenticationFailed(_)) => {
            (
                StatusCode::OK,
                Html("<div style=\"background-color: #e74c3c; color: white; padding: 0.75rem; border-radius: 0.25rem; margin-bottom: 1rem;\">Invalid credentials</div>"),
            )
                .into_response()
        }
        Err(e) => {
            error!("Failed to authenticate with upstream: {}", e);
            (
                StatusCode::OK,
                Html(format!(
                    "<div style=\"background-color: #e74c3c; color: white; padding: 0.75rem; border-radius: 0.25rem; margin-bottom: 1rem;\">Connection error: {}</div>",
                    e
                )),
            )
                .into_response()
        }
    }
}

/// Delete server admin
pub async fn delete_server_admin(
    State(state): State<AppState>,
    Path(server_id): Path<i64>,
) -> Response {
    match state.server_storage.delete_server_admin(server_id).await {
        Ok(true) => {
            info!("Deleted admin for server ID: {}", server_id);
            get_server_list(State(state)).await.into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Html("<div class=\"alert alert-error\">Admin not found</div>"),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to delete server admin: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to delete admin</div>"),
            )
                .into_response()
        }
    }
}
