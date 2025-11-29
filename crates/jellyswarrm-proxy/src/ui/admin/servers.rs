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
    encryption::encrypt_password, server_storage::Server, url_helper::join_server_url, AppState,
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
    pub password: String,
}

#[derive(Deserialize)]
struct AuthResponse {
    #[serde(rename = "User")]
    user: JellyfinUser,
}

#[derive(Deserialize)]
struct JellyfinUser {
    #[serde(rename = "Policy")]
    policy: UserPolicy,
}

#[derive(Deserialize)]
struct UserPolicy {
    #[serde(rename = "IsAdministrator")]
    is_administrator: bool,
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
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();

    let auth_url = join_server_url(&server.url, "/Users/AuthenticateByName");
    let body = serde_json::json!({
        "Username": form.username,
        "Pw": form.password
    });

    let auth_header = format!(
        "MediaBrowser Client=\"Jellyswarrm Proxy\", Device=\"Server\", DeviceId=\"jellyswarrm-proxy\", Version=\"{}\"",
        env!("CARGO_PKG_VERSION")
    );

    match client
        .post(auth_url.as_str())
        .header("Authorization", auth_header)
        .json(&body)
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => {
            // Parse response to check if user is admin
            let auth_response = match response.json::<AuthResponse>().await {
                Ok(r) => r,
                Err(e) => {
                    error!("Failed to parse auth response: {}", e);
                    return (
                        StatusCode::OK,
                        Html("<div style=\"background-color: #e74c3c; color: white; padding: 0.75rem; border-radius: 0.25rem; margin-bottom: 1rem;\">Invalid server response</div>"),
                    )
                        .into_response();
                }
            };

            if !auth_response.user.policy.is_administrator {
                return (
                    StatusCode::OK,
                    Html("<div style=\"background-color: #e74c3c; color: white; padding: 0.75rem; border-radius: 0.25rem; margin-bottom: 1rem;\">User is not an administrator on this server</div>"),
                )
                    .into_response();
            }

            // 3. Encrypt password with admin master password
            let config = state.config.read().await;
            let encrypted_password = match encrypt_password(&form.password, &config.password) {
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
        Ok(response) => {
            let status = response.status();
            if status == StatusCode::UNAUTHORIZED {
                (
                    StatusCode::OK,
                    Html("<div style=\"background-color: #e74c3c; color: white; padding: 0.75rem; border-radius: 0.25rem; margin-bottom: 1rem;\">Invalid credentials</div>"),
                )
                    .into_response()
            } else {
                (
                    StatusCode::OK,
                    Html(format!(
                        "<div style=\"background-color: #e74c3c; color: white; padding: 0.75rem; border-radius: 0.25rem; margin-bottom: 1rem;\">Upstream error: {}</div>",
                        status
                    )),
                )
                    .into_response()
            }
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
