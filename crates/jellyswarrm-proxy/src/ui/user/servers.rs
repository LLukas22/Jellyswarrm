use askama::Template;
use axum::{
    extract::{Path, State},
    http::{HeaderValue, StatusCode},
    response::{Html, IntoResponse},
    Form,
};
use serde::Deserialize;
use tracing::{error, info};

use crate::{
    models::Authorization, server_storage::Server, ui::auth::AuthenticatedUser,
    url_helper::join_server_url, AppState,
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
    pub password: String,
}

#[derive(Template)]
#[template(path = "user/user_server_status.html")]
pub struct UserServerStatusTemplate {
    pub username: Option<String>,
    pub error_message: Option<String>,
    pub needs_login: bool,
}

#[derive(Deserialize)]
struct AuthResponse {
    #[serde(rename = "AccessToken")]
    access_token: String,
    #[serde(rename = "User")]
    user: JellyfinUser,
}

#[derive(Deserialize)]
struct JellyfinUser {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Name")]
    name: String,
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
            // Parse response to get token and user ID
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

            // Credentials valid, create mapping
            match state
                .user_authorization
                .add_server_mapping(
                    &user.id,
                    server.url.as_str(),
                    &form.username,
                    &form.password,
                    None,
                )
                .await
            {
                Ok(_) => {
                    info!(
                        "Created mapping for user {} to server {}",
                        user.username, server.name
                    );

                    // Create authorization session
                    let auth = Authorization {
                        client: "Jellyswarrm Proxy".to_string(),
                        device: "Server".to_string(),
                        device_id: "jellyswarrm-proxy".to_string(),
                        version: env!("CARGO_PKG_VERSION").to_string(),
                        token: None,
                    };

                    if let Err(e) = state
                        .user_authorization
                        .store_authorization_session(
                            &user.id,
                            server.url.as_str(),
                            &auth,
                            auth_response.access_token,
                            auth_response.user.id,
                            None,
                        )
                        .await
                    {
                        error!("Failed to store session: {}", e);
                        // Continue anyway, as mapping was created
                    }

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
                    Html(format!("<div style=\"background-color: #e74c3c; color: white; padding: 0.75rem; border-radius: 0.25rem; margin-bottom: 1rem;\">Upstream error: {}</div>", status)),
                )
                    .into_response()
            }
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

    // Check for existing session
    let sessions = match state
        .user_authorization
        .get_user_sessions(&user.id, None)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to get sessions: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<span style=\"color: #dc3545;\">Database error</span>"),
            )
                .into_response();
        }
    };

    // Find session for this server
    let session = sessions
        .iter()
        .filter(|(_, s)| s.id == server.id)
        .max_by_key(|(auth, _)| auth.updated_at);

    if let Some((auth, _)) = session {
        // Try to get profile with token
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();

        let profile_url = join_server_url(&server.url, "/Users/Me");

        let auth_header = format!(
            "MediaBrowser Client=\"Jellyswarrm Proxy\", Device=\"Server\", DeviceId=\"jellyswarrm-proxy\", Version=\"{}\", Token=\"{}\"",
            env!("CARGO_PKG_VERSION"),
            auth.jellyfin_token
        );

        match client
            .get(profile_url.as_str())
            .header("Authorization", auth_header)
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => {
                match response.json::<JellyfinUser>().await {
                    Ok(profile) => {
                        let template = UserServerStatusTemplate {
                            username: Some(profile.name),
                            error_message: None,
                            needs_login: false,
                        };
                        match template.render() {
                            Ok(html) => return Html(html).into_response(),
                            Err(e) => error!("Template error: {}", e),
                        }
                    }
                    Err(e) => error!("Failed to parse profile: {}", e),
                }
            }
            _ => {
                // Token might be expired or invalid, fall through to public check
            }
        }
    }

    // Fallback: Check if server is online (public info)
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();

    let status_url = join_server_url(&server.url, "/system/info/public");

    let (msg, needs_login) = match client.get(status_url.as_str()).send().await {
        Ok(resp) if resp.status().is_success() => ("Online".to_string(), true),
        Ok(resp) => (format!("HTTP {}", resp.status().as_u16()), false),
        Err(_) => ("Offline".to_string(), false),
    };

    let template = UserServerStatusTemplate {
        username: None,
        error_message: Some(msg),
        needs_login,
    };

    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}
