use askama::Template;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse},
};
use serde::Deserialize;
use tracing::error;

use crate::{ui::auth::AuthenticatedUser, url_helper::join_server_url, AppState};

#[derive(Deserialize, Clone)]
pub struct MediaFolder {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "CollectionType")]
    pub collection_type: Option<String>,
}

#[derive(Deserialize)]
struct MediaFoldersResponse {
    #[serde(rename = "Items")]
    items: Vec<MediaFolder>,
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
}

pub struct ServerLibraries {
    pub server_name: String,
    pub libraries: Vec<MediaFolder>,
    pub error: Option<String>,
}

#[derive(Template)]
#[template(path = "user/user_media.html")]
pub struct UserMediaTemplate {
    pub servers: Vec<ServerLibraries>,
}

pub async fn get_user_media(
    State(state): State<AppState>,
    AuthenticatedUser(user): AuthenticatedUser,
) -> impl IntoResponse {
    let servers = match state.user_authorization.get_mapped_servers(&user.id).await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to list mapped servers: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response();
        }
    };

    let mut server_libraries: Vec<ServerLibraries> = Vec::new();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();

    // Get all sessions for user once
    let sessions = match state
        .user_authorization
        .get_user_sessions(&user.id, None)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to get sessions: {}", e);
            Vec::new()
        }
    };

    for server in servers {
        let mut libraries = Vec::new();
        let mut error_msg = None;

        // Find session for this server
        let session = sessions
            .iter()
            .filter(|(_, s)| s.id == server.id)
            .max_by_key(|(auth, _)| auth.updated_at);

        let mut token = session.map(|(auth, _)| auth.jellyfin_token.clone());

        // 1. Try to use existing token if available
        if let Some(t) = &token {
            let url = join_server_url(&server.url, "/Library/MediaFolders");
            let auth_header = format!(
                "MediaBrowser Client=\"Jellyswarrm Proxy\", Device=\"Server\", DeviceId=\"jellyswarrm-proxy\", Version=\"{}\", Token=\"{}\"",
                env!("CARGO_PKG_VERSION"),
                t
            );

            match client
                .get(url.as_str())
                .header("Authorization", auth_header)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    match resp.json::<MediaFoldersResponse>().await {
                        Ok(folders) => {
                            libraries = folders.items;
                        }
                        Err(e) => {
                            error_msg = Some(format!("Failed to parse: {}", e));
                        }
                    }
                }
                Ok(resp) if resp.status() == StatusCode::FORBIDDEN || resp.status() == StatusCode::UNAUTHORIZED => {
                    // Token expired, clear it to trigger re-login
                    token = None;
                }
                Ok(resp) => {
                    error_msg = Some(format!("HTTP {}", resp.status()));
                }
                Err(e) => {
                    error_msg = Some(format!("Network error: {}", e));
                }
            }
        }

        // 2. If no token or expired, try to login using mapping
        if token.is_none() && (libraries.is_empty() && error_msg.is_none() || error_msg.as_deref() == Some("HTTP 401") || error_msg.as_deref() == Some("HTTP 403")) {
            // Clear previous error if we are retrying
            error_msg = None;

            match state.user_authorization.get_server_mapping(&user.id, &server.url.as_str()).await {
                Ok(Some(mapping)) => {
                    // Decrypt password
                    let config = state.config.read().await;
                    let admin_password = &config.password;
                    
                    let decrypted_password = state.user_authorization.decrypt_server_mapping_password(
                        &mapping,
                        &user.password,
                        admin_password
                    );

                    // Perform login
                    let auth_url = join_server_url(&server.url, "/Users/AuthenticateByName");
                    let body = serde_json::json!({
                        "Username": mapping.mapped_username,
                        "Pw": decrypted_password
                    });

                    let auth_header = format!(
                        "MediaBrowser Client=\"Jellyswarrm Proxy\", Device=\"Server\", DeviceId=\"jellyswarrm-proxy\", Version=\"{}\"",
                        env!("CARGO_PKG_VERSION")
                    );

                    match client.post(auth_url.as_str()).header("Authorization", auth_header).json(&body).send().await {
                        Ok(resp) if resp.status().is_success() => {
                            match resp.json::<AuthResponse>().await {
                                Ok(auth_resp) => {
                                    // Store new session
                                    let auth = crate::models::Authorization {
                                        client: "Jellyswarrm Proxy".to_string(),
                                        device: "Server".to_string(),
                                        device_id: "jellyswarrm-proxy".to_string(),
                                        version: env!("CARGO_PKG_VERSION").to_string(),
                                        token: None,
                                    };

                                    if let Err(e) = state.user_authorization.store_authorization_session(
                                        &user.id,
                                        server.url.as_str(),
                                        &auth,
                                        auth_resp.access_token.clone(),
                                        auth_resp.user.id,
                                        None
                                    ).await {
                                        error!("Failed to store session: {}", e);
                                    }

                                    // Fetch libraries with new token
                                    let url = join_server_url(&server.url, "/Library/MediaFolders");
                                    let auth_header = format!(
                                        "MediaBrowser Client=\"Jellyswarrm Proxy\", Device=\"Server\", DeviceId=\"jellyswarrm-proxy\", Version=\"{}\", Token=\"{}\"",
                                        env!("CARGO_PKG_VERSION"),
                                        auth_resp.access_token
                                    );

                                    match client.get(url.as_str()).header("Authorization", auth_header).send().await {
                                        Ok(resp) if resp.status().is_success() => {
                                            match resp.json::<MediaFoldersResponse>().await {
                                                Ok(folders) => {
                                                    libraries = folders.items;
                                                }
                                                Err(e) => error_msg = Some(format!("Failed to parse: {}", e)),
                                            }
                                        }
                                        Ok(resp) => error_msg = Some(format!("HTTP {}", resp.status())),
                                        Err(e) => error_msg = Some(format!("Network error: {}", e)),
                                    }
                                }
                                Err(e) => error_msg = Some(format!("Login response error: {}", e)),
                            }
                        }
                        Ok(resp) => error_msg = Some(format!("Login failed: HTTP {}", resp.status())),
                        Err(e) => error_msg = Some(format!("Login network error: {}", e)),
                    }
                }
                Ok(None) => {
                    error_msg = Some("Not connected".to_string());
                }
                Err(e) => {
                    error_msg = Some(format!("Database error: {}", e));
                }
            }
        }

        server_libraries.push(ServerLibraries {
            server_name: server.name.clone(),
            libraries,
            error: error_msg,
        });
    }

    let template = UserMediaTemplate {
        servers: server_libraries,
    };

    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render user media template: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}
