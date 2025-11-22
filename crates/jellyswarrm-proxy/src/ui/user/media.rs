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
                    let error_msg = if resp.status() == StatusCode::FORBIDDEN
                        || resp.status() == StatusCode::UNAUTHORIZED
                    {
                        "Session expired, please reconnect".to_string()
                    } else {
                        format!("HTTP {}", resp.status())
                    };
                    server_libraries.push(ServerLibraries {
                        server_name: server.name.clone(),
                        libraries: Vec::new(),
                        error: Some(error_msg),
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
