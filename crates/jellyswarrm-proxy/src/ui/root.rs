use askama::Template;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse},
};
use tracing::error;

use crate::{
    ui::{JellyfinUiVersion, JELLYFIN_UI_VERSION},
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

/// Root/home page
pub async fn index(State(state): State<AppState>) -> impl IntoResponse {
    let template = IndexTemplate {
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
