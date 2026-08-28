use askama::Template;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
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
    pub auto_create_users_on_login: bool,
    pub deduplicate_movies: bool,
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
        auto_create_users_on_login: cfg.auto_create_users_on_login,
        deduplicate_movies: cfg.deduplicate_movies,
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
    #[serde(default)]
    pub auto_create_users_on_login: bool,
    #[serde(default)]
    pub deduplicate_movies: bool,
}

pub async fn save_settings(State(state): State<AppState>, Form(form): Form<SaveForm>) -> Response {
    if form.public_address.trim().is_empty() || form.server_name.trim().is_empty() {
        return Html(
            "<div id=\"settings-messages\" class=\"alert alert-error\">All fields required</div>",
        )
        .into_response();
    }

    let save_result = {
        let mut cfg = state.config.write().await;
        let mut updated = cfg.clone();
        updated.public_address = form.public_address.trim().to_string();
        updated.server_name = form.server_name.trim().to_string();
        updated.include_server_name_in_media = form.include_server_name_in_media;
        updated.auto_create_users_on_login = form.auto_create_users_on_login;
        updated.deduplicate_movies = form.deduplicate_movies;
        match save_config(&updated) {
            Ok(()) => {
                *cfg = updated;
                Ok(())
            }
            Err(error) => Err(error),
        }
    };
    if let Err(error) = save_result {
        error!("Failed to save settings: {error}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html("<div id=\"settings-messages\" class=\"alert alert-error\">Could not save settings. The previous configuration is still active.</div>"),
        )
            .into_response();
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_form_does_not_render_library_merging_control() {
        let html = SettingsFormTemplate {
            server_id: "server".to_string(),
            public_address: "http://localhost:8096".to_string(),
            server_name: "Jellyswarrm".to_string(),
            include_server_name_in_media: false,
            auto_create_users_on_login: true,
            deduplicate_movies: true,
            ui_route: "admin".to_string(),
        }
        .render()
        .unwrap();

        assert!(!html.contains("name=\"merge_libraries\""));
    }

    #[test]
    fn settings_form_renders_movie_deduplication_control() {
        let html = SettingsFormTemplate {
            server_id: "server".to_string(),
            public_address: "http://localhost:8096".to_string(),
            server_name: "Jellyswarrm".to_string(),
            include_server_name_in_media: false,
            auto_create_users_on_login: true,
            deduplicate_movies: true,
            ui_route: "admin".to_string(),
        }
        .render()
        .unwrap();

        assert!(html.contains("name=\"deduplicate_movies\""));
        assert!(html.contains("name=\"deduplicate_movies\" value=\"true\" checked"));
    }
}
