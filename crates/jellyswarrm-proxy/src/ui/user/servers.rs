use askama::Template;
use axum::{
    extract::{Path, State},
    response::{Html, IntoResponse},
    Form,
};
use hyper::{header::HeaderValue, StatusCode};
use jellyfin_api::JellyfinClient;
use serde::Deserialize;
use tracing::{error, info};

use crate::{
    encryption::{decrypt_password, HashedPassword, Password},
    server_storage::Server,
    ui::{auth::AuthenticatedUser, user::common::authenticate_user_on_server},
    AppState,
};

#[derive(Template)]
#[template(path = "user/user_server_list.html")]
pub struct UserServerListTemplate {
    pub username: String,
    pub servers: Vec<Server>,
    pub unmapped_servers: Vec<Server>,
    pub federated_server_ids: Vec<i64>,
    pub ui_route: String,
}

#[derive(Deserialize)]
pub struct ConnectServerForm {
    pub username: String,
    pub password: Password,
}

#[derive(Deserialize)]
pub struct CreateAccountForm {
    pub username: String,
    pub password: Password,
}

#[derive(Template)]
#[template(path = "user/user_server_status.html")]
pub struct UserServerStatusTemplate {
    pub username: Option<String>,
    pub error_message: Option<String>,
    pub server_version: String,
}

pub async fn get_user_servers(
    State(state): State<AppState>,
    AuthenticatedUser(user): AuthenticatedUser,
) -> impl IntoResponse {
    let mapped_servers = match state.user_authorization.get_mapped_servers(&user.id).await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to list mapped servers: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response();
        }
    };

    let all_servers = match state.server_storage.list_servers().await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to list all servers: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response();
        }
    };

    let unmapped_servers: Vec<Server> = all_servers
        .into_iter()
        .filter(|s| !mapped_servers.iter().any(|ms| ms.id == s.id))
        .collect();

    let mut federated_server_ids = Vec::new();
    for server in &unmapped_servers {
        if let Ok(Some(_)) = state.server_storage.get_server_admin(server.id).await {
            federated_server_ids.push(server.id);
        }
    }

    let template = UserServerListTemplate {
        username: user.username,
        servers: mapped_servers,
        unmapped_servers,
        federated_server_ids,
        ui_route: state.get_ui_route().await,
    };

    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render user server list template: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

pub async fn connect_server(
    State(state): State<AppState>,
    AuthenticatedUser(user): AuthenticatedUser,
    Path(server_id): Path<i64>,
    Form(form): Form<ConnectServerForm>,
) -> impl IntoResponse {
    // Get server details
    let server = match state.server_storage.get_server_by_id(server_id).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Html("<span style=\"color: #dc3545;\">Server not found</span>"),
            )
                .into_response()
        }
        Err(e) => {
            error!("Failed to get server: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<span style=\"color: #dc3545;\">Database error</span>"),
            )
                .into_response();
        }
    };

    // Verify credentials with upstream Jellyfin
    let server_url = server.url.clone();

    let client_info = crate::config::CLIENT_INFO.clone();

    let client = match JellyfinClient::new(server_url.as_str(), client_info) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to create jellyfin client: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<span style=\"color: #dc3545;\">Client error</span>"),
            )
                .into_response();
        }
    };

    match client.authenticate_by_name(&form.username, form.password.as_str()).await {
        Ok(_) => {
            let previous_mapping = match state
                .user_authorization
                .get_server_mapping(&user.id, server.url.as_str())
                .await
            {
                Ok(mapping) => mapping,
                Err(e) => {
                    error!("Failed to inspect existing mapping: {}", e);
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Html("<span style=\"color: #dc3545;\">Database error</span>"),
                    )
                        .into_response();
                }
            };

            let mapped_username_changed = previous_mapping.as_ref().is_some_and(|mapping| {
                !mapping
                    .mapped_username
                    .trim()
                    .eq_ignore_ascii_case(form.username.trim())
            });

            // Credentials valid, create mapping
            match state
                .user_authorization
                .add_server_mapping(
                    &user.id,
                    server.url.as_str(),
                    &form.username,
                    &form.password,
                    Some(&user.password_hash),
                )
                .await
            {
                Ok(mapping_id) => {
                    if mapped_username_changed {
                        match state
                            .user_authorization
                            .delete_sessions_for_mapping(mapping_id)
                            .await
                        {
                            Ok(deleted) => info!(
                                "Mapped account changed for user {} on server {}. Deleted {} affected session(s)",
                                user.username, server.name, deleted
                            ),
                            Err(e) => error!(
                                "Failed to delete sessions for updated mapping {}: {}",
                                mapping_id, e
                            ),
                        }
                    }

                    info!(
                        "Created mapping for user {} to server {}",
                        user.username, server.name
                    );

                    // Return HX-Redirect header for HTMX
                    let mut response = StatusCode::OK.into_response();
                    response.headers_mut().insert(
                        "HX-Redirect",
                        HeaderValue::from_str(&format!("/{}", state.get_ui_route().await)).unwrap(),
                    );
                    response
                },
                 Err(e) => {
                     error!("Failed to create mapping: {}", e);
                     (
                         StatusCode::OK,
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
                Html(format!("<div style=\"background-color: #e74c3c; color: white; padding: 0.75rem; border-radius: 0.25rem; margin-bottom: 1rem;\">Connection error: {}</div>", e)),
            )
                .into_response()
        }
    }
}

pub async fn create_account_on_server(
    State(state): State<AppState>,
    AuthenticatedUser(user): AuthenticatedUser,
    Path(server_id): Path<i64>,
    Form(form): Form<CreateAccountForm>,
) -> impl IntoResponse {
    let error_html = |msg: &str| -> axum::response::Response {
        (
            StatusCode::OK,
            Html(format!(
                "<div style=\"background-color: #e74c3c; color: white; padding: 0.75rem; border-radius: 0.25rem; margin-bottom: 1rem;\">{}</div>",
                msg
            )),
        )
            .into_response()
    };

    // Get server details
    let server = match state.server_storage.get_server_by_id(server_id).await {
        Ok(Some(s)) => s,
        Ok(None) => return error_html("Server not found"),
        Err(e) => {
            error!("Failed to get server: {}", e);
            return error_html("Database error");
        }
    };

    // Get admin credentials for this server
    let admin = match state.server_storage.get_server_admin(server.id).await {
        Ok(Some(a)) => a,
        Ok(None) => return error_html("This server does not support account creation"),
        Err(e) => {
            error!("Failed to get admin credentials: {}", e);
            return error_html("Database error");
        }
    };

    // Decrypt admin password
    let config = state.config.read().await;
    let admin_password: HashedPassword = config.password.clone().into();
    drop(config);

    let decrypted_admin_password = match decrypt_password(&admin.password, &admin_password) {
        Ok(p) => p,
        Err(e) => {
            error!(
                "Failed to decrypt admin password for server {}: {}",
                server.name, e
            );
            return error_html("Failed to decrypt admin credentials");
        }
    };

    let client_info = crate::config::CLIENT_INFO.clone();

    // Create client and authenticate as admin
    let client = match JellyfinClient::new(server.url.as_str(), client_info) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to create jellyfin client: {}", e);
            return error_html("Client error");
        }
    };

    if let Err(e) = client
        .authenticate_by_name(&admin.username, decrypted_admin_password.as_str())
        .await
    {
        error!(
            "Failed to authenticate as admin on server {}: {}",
            server.name, e
        );
        return error_html("Admin authentication failed");
    }

    // Create the user on the Jellyfin server
    match client
        .create_user(&form.username, Some(form.password.as_str()))
        .await
    {
        Ok(_new_user) => {
            // Create local server mapping
            match state
                .user_authorization
                .add_server_mapping(
                    &user.id,
                    server.url.as_str(),
                    &form.username,
                    &form.password,
                    Some(&user.password_hash),
                )
                .await
            {
                Ok(_) => {
                    info!(
                        "Created account and mapping for user {} on server {}",
                        form.username, server.name
                    );
                    let mut response = StatusCode::OK.into_response();
                    response.headers_mut().insert(
                        "HX-Redirect",
                        HeaderValue::from_str(&format!("/{}", state.get_ui_route().await)).unwrap(),
                    );
                    response
                }
                Err(e) => {
                    error!("Failed to create mapping: {}", e);
                    error_html("Account created but failed to save mapping")
                }
            }
        }
        Err(e) => {
            error!("Failed to create user on server {}: {}", server.name, e);
            error_html(&format!("Failed to create account: {}", e))
        }
    }
}

pub async fn delete_server_mapping(
    State(state): State<AppState>,
    AuthenticatedUser(user): AuthenticatedUser,
    Path(server_id): Path<i64>,
) -> impl IntoResponse {
    // Get server details to find the URL
    let server = match state.server_storage.get_server_by_id(server_id).await {
        Ok(Some(s)) => s,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            error!("Failed to get server: {}", e);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Find the mapping
    let mappings = match state
        .user_authorization
        .list_server_mappings(&user.id)
        .await
    {
        Ok(m) => m,
        Err(e) => {
            error!("Failed to list mappings: {}", e);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Normalize URLs for comparison (remove trailing slashes)
    let server_url = server.url.as_str().trim_end_matches('/');

    if let Some(mapping) = mappings
        .iter()
        .find(|m| m.server_url.trim_end_matches('/') == server_url)
    {
        match state
            .user_authorization
            .delete_server_mapping(mapping.id)
            .await
        {
            Ok(_) => {
                info!(
                    "Deleted mapping for user {} to server {}",
                    user.username, server.name
                );
                // Return HX-Redirect header for HTMX
                let mut response = StatusCode::OK.into_response();
                response.headers_mut().insert(
                    "HX-Redirect",
                    HeaderValue::from_str(&format!("/{}", state.get_ui_route().await)).unwrap(),
                );
                return response;
            }
            Err(e) => {
                error!("Failed to delete mapping: {}", e);
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    }

    StatusCode::NOT_FOUND.into_response()
}

pub async fn check_user_server_status(
    State(state): State<AppState>,
    AuthenticatedUser(user): AuthenticatedUser,
    Path(server_id): Path<i64>,
) -> impl IntoResponse {
    // Get server details
    let server = match state.server_storage.get_server_by_id(server_id).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Html("<span style=\"color: #dc3545;\">Server not found</span>"),
            )
                .into_response()
        }
        Err(e) => {
            error!("Failed to get server: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<span style=\"color: #dc3545;\">Database error</span>"),
            )
                .into_response();
        }
    };

    match authenticate_user_on_server(&state, &user, &server).await {
        Ok((_client, jellyfin_user, public_info)) => {
            let template = UserServerStatusTemplate {
                username: Some(jellyfin_user.name),
                error_message: None,
                server_version: public_info.version.unwrap_or("unknown".to_string()),
            };
            match template.render() {
                Ok(html) => Html(html).into_response(),
                Err(e) => {
                    error!("Failed to render user server status template: {}", e);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Html("<span style=\"color: #dc3545;\">Template error</span>"),
                    )
                        .into_response()
                }
            }
        }
        Err(e) => (
            StatusCode::OK,
            Html(format!("<span style=\"color: #dc3545;\">{}</span>", e)),
        )
            .into_response(),
    }
}
