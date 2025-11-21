use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse},
};
use serde::Deserialize;
use tracing::error;

use crate::{server_storage::Server, ui::auth::AuthSession, url_helper::join_server_url, AppState};

#[derive(Template)]
#[template(path = "user/user_server_list.html")]
pub struct UserServerListTemplate {
    pub username: String,
    pub servers: Vec<Server>,
    pub ui_route: String,
}

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

#[derive(Template)]
#[template(path = "user/user_server_status.html")]
pub struct UserServerStatusTemplate {
    pub username: Option<String>,
    pub error_message: Option<String>,
}

#[derive(Deserialize)]
struct JellyfinUser {
    #[serde(rename = "Name")]
    name: String,
}

pub async fn get_user_servers(
    State(state): State<AppState>,
    auth_session: AuthSession,
) -> impl IntoResponse {
    let user = match auth_session.user {
        Some(user) => user,
        None => return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response(),
    };

    let servers = match state.user_authorization.get_mapped_servers(&user.id).await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to list mapped servers: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response();
        }
    };

    let template = UserServerListTemplate {
        username: user.username,
        servers,
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

pub async fn get_user_media(
    State(state): State<AppState>,
    auth_session: AuthSession,
) -> impl IntoResponse {
    let user = match auth_session.user {
        Some(user) => user,
        None => return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response(),
    };

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
        // Find session for this server
        let session = sessions
            .iter()
            .filter(|(_, s)| s.id == server.id)
            .max_by_key(|(auth, _)| auth.updated_at);

        if let Some((auth, _)) = session {
            let url = join_server_url(&server.url, "/Library/MediaFolders");
            let auth_header = format!(
                "MediaBrowser Client=\"Jellyswarrm Proxy\", Device=\"Server\", DeviceId=\"jellyswarrm-proxy\", Version=\"{}\", Token=\"{}\"",
                env!("CARGO_PKG_VERSION"),
                auth.jellyfin_token
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
                            server_libraries.push(ServerLibraries {
                                server_name: server.name.clone(),
                                libraries: folders.items,
                                error: None,
                            });
                        }
                        Err(e) => {
                            server_libraries.push(ServerLibraries {
                                server_name: server.name.clone(),
                                libraries: Vec::new(),
                                error: Some(format!("Failed to parse: {}", e)),
                            });
                        }
                    }
                }
                Ok(resp) => {
                    server_libraries.push(ServerLibraries {
                        server_name: server.name.clone(),
                        libraries: Vec::new(),
                        error: Some(format!("HTTP {}", resp.status())),
                    });
                }
                Err(e) => {
                    server_libraries.push(ServerLibraries {
                        server_name: server.name.clone(),
                        libraries: Vec::new(),
                        error: Some(format!("Network error: {}", e)),
                    });
                }
            }
        } else {
            server_libraries.push(ServerLibraries {
                server_name: server.name.clone(),
                libraries: Vec::new(),
                error: Some("Not connected".to_string()),
            });
        }
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

pub async fn check_user_server_status(
    State(state): State<AppState>,
    auth_session: AuthSession,
    Path(server_id): Path<i64>,
) -> impl IntoResponse {
    let user = match auth_session.user {
        Some(user) => user,
        None => return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response(),
    };

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

    let msg = match client.get(status_url.as_str()).send().await {
        Ok(resp) if resp.status().is_success() => "Online".to_string(),
        Ok(resp) => format!("HTTP {}", resp.status().as_u16()),
        Err(_) => "Offline".to_string(),
    };

    let template = UserServerStatusTemplate {
        username: None,
        error_message: Some(msg),
    };

    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}
