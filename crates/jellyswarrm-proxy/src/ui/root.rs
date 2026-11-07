//! Render the management UI landing page and its common layout context.
use askama::Template;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse},
};
use tracing::{error, info};

use crate::{
    http::{
        assets::{JellyfinUiVersion, JELLYFIN_UI_VERSION},
        state::AdapterPaths,
    },
    ui::auth::{AuthenticatedUser, UserRole},
};

#[derive(Template)]
#[template(path = "user/index.html")]
pub(crate) struct UserIndexTemplate {
    pub(crate) version: Option<String>,
    pub(crate) ui_route: String,
    pub(crate) root: Option<String>,
    pub(crate) jellyfin_ui_version: Option<JellyfinUiVersion>,
}

#[derive(Template)]
#[template(path = "admin/index.html")]
pub(crate) struct AdminIndexTemplate {
    pub(crate) version: Option<String>,
    pub(crate) ui_route: String,
    pub(crate) root: Option<String>,
    pub(crate) jellyfin_ui_version: Option<JellyfinUiVersion>,
}

/// Root/home page
pub(crate) async fn index(
    State(state): State<AdapterPaths>,
    AuthenticatedUser(user): AuthenticatedUser,
) -> impl IntoResponse {
    let response = if user.role == UserRole::User {
        let template = UserIndexTemplate {
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
            ui_route: state.ui_route(),
            root: state.url_prefix(),
            jellyfin_ui_version: JELLYFIN_UI_VERSION.clone(),
        };

        match template.render() {
            Ok(html) => Html(html).into_response(),
            Err(e) => {
                error!("Failed to render index template: {}", e);
                (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
            }
        }
    } else {
        info!("Rendering admin dashboard for {}", user.username);
        let template = AdminIndexTemplate {
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
            ui_route: state.ui_route(),
            root: state.url_prefix(),
            jellyfin_ui_version: JELLYFIN_UI_VERSION.clone(),
        };

        match template.render() {
            Ok(html) => Html(html).into_response(),
            Err(e) => {
                error!("Failed to render index template: {}", e);
                (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
            }
        }
    };
    response
}
