use askama::Template;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse},
};
use jellyfin_api::models::MediaFolder;
use tracing::error;

use crate::{ui::auth::AuthenticatedUser, AppState};

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

        let token = session.map(|(auth, _)| auth.jellyfin_token.clone());

        let client_info = crate::config::CLIENT_INFO.clone();

        let client = match jellyfin_api::JellyfinClient::new(server.url.as_str(), client_info) {
            Ok(c) => c,
            Err(e) => {
                server_libraries.push(ServerLibraries {
                    server_name: server.name.clone(),
                    libraries: Vec::new(),
                    error: Some(format!("Client error: {}", e)),
                });
                continue;
            }
        };

        // 1. Try to use existing token if available
        let mut needs_reauth = true;

        if let Some(t) = &token {
            let client_with_token = client.clone().with_token(t.clone());
            match client_with_token.get_media_folders().await {
                Ok(folders) => {
                    libraries = folders;
                    needs_reauth = false;
                }
                Err(jellyfin_api::error::Error::AuthenticationFailed(_)) => {
                    // Token expired, needs reauth
                    needs_reauth = true;
                }
                Err(e) => {
                    error_msg = Some(format!("Error: {}", e));
                    needs_reauth = false;
                }
            }
        }

        // 2. If no token or expired, try to login using mapping
        if needs_reauth && error_msg.is_none() {
            match state
                .user_authorization
                .get_server_mapping(&user.id, server.url.as_str())
                .await
            {
                Ok(Some(mapping)) => {
                    // Decrypt password
                    let config = state.config.read().await;
                    let admin_password = &config.password;

                    let decrypted_password = state
                        .user_authorization
                        .decrypt_server_mapping_password(&mapping, &user.password, admin_password);

                    match client
                        .authenticate_by_name(&mapping.mapped_username, &decrypted_password)
                        .await
                    {
                        Ok(user_info) => {
                            // Store new session
                            let auth = crate::models::Authorization {
                                client: "Jellyswarrm Proxy".to_string(),
                                device: "Server".to_string(),
                                device_id: "jellyswarrm-proxy".to_string(),
                                version: env!("CARGO_PKG_VERSION").to_string(),
                                token: None,
                            };

                            if let Some(new_token) = client.get_token() {
                                if let Err(e) = state
                                    .user_authorization
                                    .store_authorization_session(
                                        &user.id,
                                        server.url.as_str(),
                                        &auth,
                                        new_token.to_string(),
                                        user_info.id,
                                        None,
                                    )
                                    .await
                                {
                                    error!("Failed to store session: {}", e);
                                }

                                // Retry fetch libraries
                                match client.get_media_folders().await {
                                    Ok(folders) => {
                                        libraries = folders;
                                    }
                                    Err(e) => {
                                        error_msg = Some(format!(
                                            "Error fetching libraries after login: {}",
                                            e
                                        ));
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            error_msg = Some(format!("Login failed: {}", e));
                        }
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
