use askama::Template;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse},
};
use tracing::error;

use crate::{
    server_storage::Server,
    ui::auth::AuthSession,
    AppState,
};

#[derive(Template)]
#[template(path = "user_dashboard.html")]
pub struct UserDashboardTemplate {
    pub username: String,
    pub servers: Vec<Server>,
    pub ui_route: String,
    pub root: Option<String>,
}

pub async fn dashboard(
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

    let template = UserDashboardTemplate {
        username: user.username,
        servers,
        ui_route: state.get_ui_route().await,
        root: state.get_url_prefix().await,
    };

    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render user dashboard template: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}
