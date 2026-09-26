use askama::Template;
use axum::{
    extract::{Path, State},
    response::{Html, IntoResponse},
    Form, Json,
};
use chrono::{DateTime, Duration, Utc};
use hyper::{header::HeaderValue, StatusCode};
use jellyfin_api::JellyfinClient;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};
use tracing::{error, info};
use uuid::Uuid;

use crate::{
    encryption::Password,
    server_id::ServerId,
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
    pub ui_route: String,
}

#[derive(Deserialize)]
pub struct ConnectServerForm {
    pub username: String,
    pub password: Password,
}

struct PendingConnect {
    user_id: String,
    server_id: ServerId,
    secret: String,
    created_at: DateTime<Utc>,
}

static PENDING_CONNECTS: LazyLock<Mutex<HashMap<String, PendingConnect>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Serialize)]
pub struct QuickConnectProgress {
    status: &'static str,
    code: Option<String>,
    request_id: Option<String>,
}

pub async fn initiate_quick_connect(
    State(state): State<AppState>,
    AuthenticatedUser(user): AuthenticatedUser,
    Path(server_id): Path<ServerId>,
) -> Result<Json<QuickConnectProgress>, StatusCode> {
    // Admins do not have a row in the local user table and cannot own mappings.
    if state
        .user_authorization
        .get_user_by_id(&user.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .is_none()
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let server = state
        .server_storage
        .get_server_by_id(server_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let client = JellyfinClient::new(
        server.url.as_str(),
        jellyfin_api::ClientInfo {
            device_id: crate::mapping_auth::mapping_device_id(&server, &user.id),
            ..crate::config::CLIENT_INFO.clone()
        },
    )
    .map_err(|_| StatusCode::BAD_GATEWAY)?;
    if !client
        .quick_connect_enabled()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?
    {
        return Err(StatusCode::CONFLICT);
    }
    let result = client
        .initiate_quick_connect()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    let request_id = Uuid::new_v4().to_string();
    let mut pending = PENDING_CONNECTS.lock().unwrap_or_else(|e| e.into_inner());
    pending.retain(|_, p| Utc::now() - p.created_at < Duration::minutes(10));
    pending.insert(
        request_id.clone(),
        PendingConnect {
            user_id: user.id,
            server_id,
            secret: result.secret,
            created_at: Utc::now(),
        },
    );
    Ok(Json(QuickConnectProgress {
        status: "pending",
        code: Some(result.code),
        request_id: Some(request_id),
    }))
}

pub async fn finish_quick_connect(
    State(state): State<AppState>,
    AuthenticatedUser(user): AuthenticatedUser,
    Path((server_id, request_id)): Path<(ServerId, String)>,
) -> Result<Json<QuickConnectProgress>, StatusCode> {
    let secret = {
        let mut pending = PENDING_CONNECTS.lock().unwrap_or_else(|e| e.into_inner());
        pending.retain(|_, p| Utc::now() - p.created_at < Duration::minutes(10));
        let p = pending.get(&request_id).ok_or(StatusCode::NOT_FOUND)?;
        if p.user_id != user.id || p.server_id != server_id {
            return Err(StatusCode::NOT_FOUND);
        }
        p.secret.clone()
    };
    let server = state
        .server_storage
        .get_server_by_id(server_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let client = JellyfinClient::new(
        server.url.as_str(),
        jellyfin_api::ClientInfo {
            device_id: crate::mapping_auth::mapping_device_id(&server, &user.id),
            ..crate::config::CLIENT_INFO.clone()
        },
    )
    .map_err(|_| StatusCode::BAD_GATEWAY)?;
    let progress = client
        .quick_connect_state(&secret)
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    if !progress.authenticated {
        return Ok(Json(QuickConnectProgress {
            status: "pending",
            code: None,
            request_id: None,
        }));
    }
    // The request ID is one-use, including when redemption fails. Retrying requires
    // a new approval rather than racing two token writes for the same mapping.
    if PENDING_CONNECTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&request_id)
        .is_none()
    {
        return Err(StatusCode::NOT_FOUND);
    }
    let auth: jellyfin_api::models::AuthResponse = client
        .authenticate_with_quick_connect(&secret)
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    if auth.access_token.is_empty() || auth.user.id.is_empty() {
        return Err(StatusCode::BAD_GATEWAY);
    }
    let encryption_key: crate::encryption::HashedPassword =
        (&state.get_admin_password().await).into();
    state
        .user_authorization
        .add_quick_connect_mapping(
            &user.id,
            &server,
            &auth.user.name,
            &auth.user.id,
            &auth.access_token,
            &encryption_key,
        )
        .await
        .map_err(|e| {
            error!("Failed to save Quick Connect mapping: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(Json(QuickConnectProgress {
        status: "connected",
        code: None,
        request_id: None,
    }))
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

    let template = UserServerListTemplate {
        username: user.username,
        servers: mapped_servers,
        unmapped_servers,
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
    Path(server_id): Path<ServerId>,
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
            // Credentials valid, create mapping
            let mapping_key = user.local_credential.mapping_key();
            match state
                .user_authorization
                .add_server_mapping(
                    &user.id,
                    &server,
                    &form.username,
                    &form.password,
                    Some(&mapping_key),
                )
                .await
            {
                Ok(_) => {
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
                }
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

pub async fn delete_server_mapping(
    State(state): State<AppState>,
    AuthenticatedUser(user): AuthenticatedUser,
    Path(server_id): Path<ServerId>,
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
    if let Some(mapping) = mappings.iter().find(|m| m.server_id == server.id) {
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
    Path(server_id): Path<ServerId>,
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
