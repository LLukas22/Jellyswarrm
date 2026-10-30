//! Render and mutate configured backend servers from the administrative UI.
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
    config::MediaStreamingMode, domain::ids::ServerId, domain::security::Password,
    http::state::ManagementState, servers::storage::Server,
};

#[derive(Template)]
#[template(path = "admin/servers.html")]
pub(crate) struct ServersPageTemplate {
    pub(crate) ui_route: String,
}

pub(crate) struct ServerWithAdmin {
    pub(crate) server: Server,
    pub(crate) has_admin: bool,
    pub(crate) is_redirect: bool,
    pub(crate) is_proxy: bool,
}

#[derive(Template)]
#[template(path = "admin/server_list.html")]
pub(crate) struct ServerListTemplate {
    pub(crate) servers: Vec<ServerWithAdmin>,
    pub(crate) ui_route: String,
}

#[derive(Deserialize)]
pub(crate) struct AddServerForm {
    pub(crate) name: String,
    pub(crate) url: String,
    pub(crate) priority: i32,
    pub(crate) media_streaming_mode: String,
}

#[derive(Deserialize)]
pub(crate) struct UpdatePriorityForm {
    pub(crate) priority: i32,
}

#[derive(Deserialize)]
pub(crate) struct UpdateMediaStreamingModeForm {
    pub(crate) media_streaming_mode: String,
}

#[derive(Deserialize)]
pub(crate) struct AddServerAdminForm {
    pub(crate) username: String,
    pub(crate) password: Password,
}

async fn render_server_list(state: &ManagementState) -> Result<String, String> {
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
                let is_redirect = server.media_streaming_mode == MediaStreamingMode::Redirect;
                servers_with_admin.push(ServerWithAdmin {
                    server,
                    has_admin,
                    is_redirect,
                    is_proxy: !is_redirect,
                });
            }

            let template = ServerListTemplate {
                servers: servers_with_admin,
                ui_route: state.paths.ui_route(),
            };

            template.render().map_err(|e| e.to_string())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Main servers management page
pub(crate) async fn servers_page(State(state): State<ManagementState>) -> impl IntoResponse {
    let template = ServersPageTemplate {
        ui_route: state.paths.ui_route(),
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
pub(crate) async fn get_server_list(State(state): State<ManagementState>) -> impl IntoResponse {
    match render_server_list(&state).await {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render server list: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Error").into_response()
        }
    }
}

/// Add a new server
pub(crate) async fn add_server(
    State(state): State<ManagementState>,
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

    let media_streaming_mode = match form.media_streaming_mode.parse::<MediaStreamingMode>() {
        Ok(mode) => mode,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Html("<div class=\"alert alert-error\">Invalid streaming mode</div>"),
            )
                .into_response()
        }
    };

    // Try to add the server
    match state
        .server_administration
        .add(
            form.name.trim(),
            form.url.trim(),
            form.priority,
            media_streaming_mode,
        )
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

            let error_message = if let sqlx::Error::Database(db_error) = &e {
                let constraint = db_error.constraint().unwrap_or_default();
                let message = db_error.message();
                if constraint == "idx_servers_url_unique"
                    || message.contains("idx_servers_url_unique")
                    || message.contains("servers.url")
                {
                    "A server with that URL already exists"
                } else if message.contains("servers.name")
                    || message.contains("UNIQUE constraint failed")
                {
                    "A server with that name already exists"
                } else {
                    "Failed to add server"
                }
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

/// Update server media streaming mode
pub(crate) async fn update_server_media_streaming_mode(
    State(state): State<ManagementState>,
    Path(server_id): Path<ServerId>,
    Form(form): Form<UpdateMediaStreamingModeForm>,
) -> Response {
    let media_streaming_mode = match form.media_streaming_mode.parse::<MediaStreamingMode>() {
        Ok(mode) => mode,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Html("<div class=\"alert alert-error\">Invalid streaming mode</div>"),
            )
                .into_response()
        }
    };

    match state
        .server_administration
        .set_streaming_mode(server_id, media_streaming_mode)
        .await
    {
        Ok(true) => {
            info!(
                "Updated server {} media streaming mode to {}",
                server_id, media_streaming_mode
            );
            get_server_list(State(state)).await.into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Html("<div class=\"alert alert-error\">Server not found</div>"),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to update server media streaming mode: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to update streaming mode</div>"),
            )
                .into_response()
        }
    }
}

/// Delete a server
pub(crate) async fn delete_server(
    State(state): State<ManagementState>,
    Path(server_id): Path<ServerId>,
) -> Response {
    match state.server_administration.delete(server_id).await {
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
pub(crate) async fn update_server_priority(
    State(state): State<ManagementState>,
    Path(server_id): Path<ServerId>,
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
        .server_administration
        .set_priority(server_id, form.priority)
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
pub(crate) async fn add_server_admin(
    State(state): State<ManagementState>,
    Path(server_id): Path<ServerId>,
    Form(form): Form<AddServerAdminForm>,
) -> Response {
    match state
        .server_credentials
        .save(server_id, &form.username, &form.password)
        .await
    {
        Ok(()) => match render_server_list(&state).await {
            Ok(html) => Html(format!(
                r#"<div id="server-list" hx-swap-oob="innerHTML">{}</div>"#,
                html
            ))
            .into_response(),
            Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
        },
        Err(error) => server_credential_error(error),
    }
}

fn server_credential_error(
    error: crate::application::server_admin::ServerCredentialError,
) -> Response {
    use crate::application::server_admin::ServerCredentialError;
    // Keep upstream failure details out of HTML (and credentials out of logs).
    let (status, message) = match error {
        ServerCredentialError::NotFound => (StatusCode::NOT_FOUND, "Server not found"),
        ServerCredentialError::NotAdministrator => (
            StatusCode::OK,
            "User is not an administrator on this server",
        ),
        ServerCredentialError::Client(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Client error"),
        ServerCredentialError::Authentication(
            jellyfin_api::error::Error::AuthenticationFailed(_),
        ) => (StatusCode::OK, "Invalid credentials"),
        ServerCredentialError::Authentication(_) => (StatusCode::OK, "Connection error"),
        ServerCredentialError::Encryption(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "Encryption failed")
        }
        ServerCredentialError::Storage(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Database error"),
    };
    (status, Html(format!("<div style=\"background-color: #e74c3c; color: white; padding: 0.75rem; border-radius: 0.25rem; margin-bottom: 1rem;\">{message}</div>"))).into_response()
}

/// Delete server admin
pub(crate) async fn delete_server_admin(
    State(state): State<ManagementState>,
    Path(server_id): Path<ServerId>,
) -> Response {
    match state.server_credentials.delete(server_id).await {
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
