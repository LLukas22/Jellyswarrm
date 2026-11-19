//! Render and save proxy settings from the administrative UI.
use askama::Template;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    Form,
};
use serde::Deserialize;
use tracing::error;

use crate::{
    config::service::{ConfigurationError, SettingsUpdate},
    http::state::SettingsState,
};

#[derive(Template)]
#[template(path = "admin/settings.html")]
pub(crate) struct SettingsPageTemplate {
    pub(crate) ui_route: String,
}

#[derive(Template)]
#[template(path = "admin/settings_form.html")]
pub(crate) struct SettingsFormTemplate {
    pub(crate) server_id: String,
    pub(crate) public_address: String,
    pub(crate) server_name: String,
    pub(crate) include_server_name_in_media: bool,
    pub(crate) auto_create_users_on_login: bool,
    pub(crate) deduplicate_media: bool,
    pub(crate) prefer_best_version: bool,
    pub(crate) ui_route: String,
}

pub(crate) async fn settings_page(State(state): State<SettingsState>) -> impl IntoResponse {
    let template = SettingsPageTemplate {
        ui_route: state.paths.ui_route(),
    };
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render settings page: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

pub(crate) async fn settings_form(State(state): State<SettingsState>) -> impl IntoResponse {
    let cfg = state.settings.read().await;
    let form = SettingsFormTemplate {
        server_id: cfg.server_id.clone(),
        public_address: cfg.public_address.clone(),
        server_name: cfg.server_name.clone(),
        include_server_name_in_media: cfg.include_server_name_in_media,
        auto_create_users_on_login: cfg.auto_create_users_on_login,
        deduplicate_media: cfg.deduplicate_media,
        prefer_best_version: cfg.prefer_best_version,
        ui_route: state.paths.ui_route(),
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
pub(crate) struct SaveForm {
    pub(crate) public_address: String,
    pub(crate) server_name: String,
    // When the checkbox is unchecked the field is absent; default to false.
    #[serde(default)]
    pub(crate) include_server_name_in_media: bool,
    #[serde(default)]
    pub(crate) auto_create_users_on_login: bool,
    #[serde(default)]
    pub(crate) deduplicate_media: bool,
    #[serde(default)]
    pub(crate) prefer_best_version: bool,
}

pub(crate) async fn save_settings(
    State(state): State<SettingsState>,
    Form(form): Form<SaveForm>,
) -> Response {
    let save_result = state
        .configuration
        .save(SettingsUpdate {
            public_address: form.public_address,
            server_name: form.server_name,
            include_server_name_in_media: form.include_server_name_in_media,
            auto_create_users_on_login: form.auto_create_users_on_login,
            deduplicate_media: form.deduplicate_media,
            prefer_best_version: form.prefer_best_version,
        })
        .await;
    if let Err(error) = save_result {
        if matches!(error, ConfigurationError::MissingFields) {
            return Html("<div id=\"settings-messages\" class=\"alert alert-error\">All fields required</div>").into_response();
        }
        error!("Failed to save settings: {error}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html("<div id=\"settings-messages\" class=\"alert alert-error\">Could not save settings. The previous configuration is still active.</div>"),
        )
            .into_response();
    }

    settings_form(State(state)).await.into_response()
}

pub(crate) async fn reload_config(State(state): State<SettingsState>) -> impl IntoResponse {
    match state.configuration.reload().await {
        Ok(()) => Html("<div class=\"alert\">Configuration reloaded</div>").into_response(),
        Err(ConfigurationError::SessionKeyChanged) => {
            (StatusCode::CONFLICT, Html("<div class=\"alert alert-error\">session_key cannot be changed while running</div>")).into_response()
        }
        Err(ConfigurationError::StartupChanged) => {
            (StatusCode::CONFLICT, Html("<div class=\"alert alert-error\">Startup settings changed; restart the application to apply them. The previous configuration is still active.</div>")).into_response()
        }
        Err(error) => {
            error!("Failed to reload config: {error}");
            (
                StatusCode::BAD_REQUEST,
                Html("<div class=\"alert alert-error\">Invalid configuration</div>"),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(prefer_best_version: bool) -> SettingsFormTemplate {
        SettingsFormTemplate {
            server_id: "server".to_string(),
            public_address: "http://localhost:8096".to_string(),
            server_name: "Jellyswarrm".to_string(),
            include_server_name_in_media: false,
            auto_create_users_on_login: true,
            deduplicate_media: true,
            prefer_best_version,
            ui_route: "admin".to_string(),
        }
    }

    #[test]
    fn settings_form_does_not_render_library_merging_control() {
        let html = form(false).render().unwrap();

        assert!(!html.contains("name=\"merge_libraries\""));
    }

    #[test]
    fn settings_form_renders_media_deduplication_control() {
        let html = form(false).render().unwrap();

        assert!(html.contains("name=\"deduplicate_media\""));
        assert!(html.contains("name=\"deduplicate_media\" value=\"true\" checked"));
    }

    #[test]
    fn settings_form_renders_best_version_control() {
        let unchecked = form(false).render().unwrap();
        assert!(unchecked.contains("name=\"prefer_best_version\" value=\"true\" >"));

        let checked = form(true).render().unwrap();
        assert!(checked.contains("name=\"prefer_best_version\" value=\"true\" checked"));
    }

    #[test]
    fn unchecked_best_version_switch_saves_as_disabled() {
        let form: SaveForm = serde_json::from_value(serde_json::json!({
            "public_address": "https://proxy",
            "server_name": "Swarm"
        }))
        .unwrap();
        assert!(!form.prefer_best_version);
        let form: SaveForm = serde_json::from_value(serde_json::json!({
            "public_address": "https://proxy",
            "server_name": "Swarm",
            "prefer_best_version": true
        }))
        .unwrap();
        assert!(form.prefer_best_version);
    }
}
