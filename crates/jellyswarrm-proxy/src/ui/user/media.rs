use askama::Template;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse},
};
use jellyfin_api::models::MediaFolder;
use tracing::error;

use crate::{
    ui::{auth::AuthenticatedUser, user::common::authenticate_user_on_server},
    AppState,
};

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

    for server in servers {
        let mut libraries = Vec::new();
        let mut error_msg = None;

        // Authenticate user on the server
        if let Ok((client, _, _)) = authenticate_user_on_server(&state, &user, &server).await {
            match client.get_media_folders().await {
                Ok(folders) => {
                    libraries = folders;
                    if let Err(e) = client.logout().await {
                        error!("Failed to logout from server {}: {}", server.name, e);
                    }
                }
                Err(e) => {
                    error!(
                        "Failed to get media folders from server {}: {}",
                        server.name, e
                    );
                    error_msg = Some(format!("Error fetching media folders: {}", e));
                }
            }
        } else {
            error_msg = Some("Failed to authenticate on server".to_string());
        }

        server_libraries.push(ServerLibraries {
            server_name: server.name,
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
