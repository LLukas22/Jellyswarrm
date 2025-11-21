use askama::Template;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse},
};
use tracing::error;

use crate::{server_storage::Server, ui::auth::AuthSession, AppState};

#[derive(Template)]
#[template(path = "user/user_server_list.html")]
pub struct UserServerListTemplate {
    pub username: String,
    pub servers: Vec<Server>,
    pub ui_route: String,
}

#[derive(Template)]
#[template(path = "user/user_media.html")]
pub struct UserMediaTemplate {}

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

pub async fn get_user_media() -> impl IntoResponse {
    let template = UserMediaTemplate {};
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render user media template: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}
