use askama::Template;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Redirect},
};
use tracing::{error, info};

use crate::{
    ui::{
        auth::{AuthSession, UserRole},
        JellyfinUiVersion, JELLYFIN_UI_VERSION,
    },
    AppState,
};

#[derive(Template)]
#[template(path = "index.html")]
pub struct IndexTemplate {
    pub version: Option<String>,
    pub ui_route: String,
    pub root: Option<String>,
    pub jellyfin_ui_version: Option<JellyfinUiVersion>,
}

#[derive(Template)]
#[template(path = "admin/index.html")]
pub struct AdminIndexTemplate {
    pub version: Option<String>,
    pub ui_route: String,
    pub root: Option<String>,
    pub jellyfin_ui_version: Option<JellyfinUiVersion>,
}

/// Root/home page
pub async fn index(
    State(state): State<AppState>,
    auth_session: AuthSession,
) -> impl IntoResponse {
    let user = match auth_session.user {
        Some(user) => user,
        None => {
            info!("No user found in session, redirecting to login");
            return Redirect::to("/ui/login").into_response();
        }
    };

    if user.role == UserRole::User {
        let ui_route = state.get_ui_route().await;
        // Remove leading slash from ui_route if present to avoid double slash
        let ui_route = ui_route.trim_start_matches('/');
        info!("Redirecting user {} to dashboard", user.username);
        return Redirect::to(&format!("/{}/dashboard", ui_route)).into_response();
    }

    info!("Rendering admin dashboard for {}", user.username);
    let template = AdminIndexTemplate {
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
        ui_route: state.get_ui_route().await,
        root: state.get_url_prefix().await,
        jellyfin_ui_version: JELLYFIN_UI_VERSION.clone(),
    };

    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render index template: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}
