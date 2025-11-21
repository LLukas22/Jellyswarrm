use askama::Template;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse},
    Form,
};
use serde::Deserialize;
use tracing::error;

use crate::{config::save_config, AppState};

#[derive(Template)]
#[template(path = "admin/settings.html")]
pub struct SettingsPageTemplate {
    pub ui_route: String,
}

#[derive(Template)]
#[template(path = "admin/settings_form.html")]
pub struct SettingsFormTemplate {
    pub server_id: String,
    pub public_address: String,
    pub server_name: String,
    pub include_server_name_in_media: bool,
    pub ui_route: String,
}

pub async fn settings_page(State(state): State<AppState>) -> impl IntoResponse {
    let template = SettingsPageTemplate {
        ui_route: state.get_ui_route().await,
    };
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render settings page: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

pub async fn settings_form(State(state): State<AppState>) -> impl IntoResponse {
    let cfg = state.config.read().await.clone();
    let form = SettingsFormTemplate {
        server_id: cfg.server_id,
        public_address: cfg.public_address,
        server_name: cfg.server_name,
        include_server_name_in_media: cfg.include_server_name_in_media,
        ui_route: state.get_ui_route().await,
    };
    match form.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render settings form: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct SaveForm {
    pub public_address: String,
    pub server_name: String,
    // When the checkbox is unchecked the field is absent; default to false.
    #[serde(default)]
    pub include_server_name_in_media: bool,
}

pub async fn save_settings(
    State(state): State<AppState>,
    Form(form): Form<SaveForm>,
) -> impl IntoResponse {
    if form.public_address.trim().is_empty() || form.server_name.trim().is_empty() {
        return Html(
            "<div id=\"settings-messages\" class=\"alert alert-error\">All fields required</div>",
        )
        .into_response();
    }
    {
        let mut cfg = state.config.write().await;
        cfg.public_address = form.public_address.trim().to_string();
        cfg.server_name = form.server_name.trim().to_string();
        cfg.include_server_name_in_media = form.include_server_name_in_media;
        if let Err(e) = save_config(&cfg) {
            error!("Save failed: {}", e);
        }
    }
    // Return fresh form (like server list pattern)
    settings_form(State(state)).await.into_response()
}

pub async fn reload_config(State(state): State<AppState>) -> impl IntoResponse {
    let new_cfg = crate::config::load_config();
    {
        let mut cfg = state.config.write().await;
        *cfg = new_cfg;
    }
    Html("<div class=\"alert\">Configuration reloaded</div>")
}
