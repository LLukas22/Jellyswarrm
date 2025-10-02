use axum::{
    body::Body,
    extract::Path,
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use axum_login::login_required;
use hyper::StatusCode;
use rust_embed::RustEmbed;

use crate::AppState;

mod auth;
pub mod root;
pub mod servers;
pub mod settings;
pub mod users;
pub use auth::Backend;

#[derive(RustEmbed)]
#[folder = "src/ui/resources/"]
struct Resources;

async fn resource_handler(Path(path): Path<String>) -> impl IntoResponse {
    if let Some(file) = Resources::get(&path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        Ok(Response::builder()
            .header("Content-Type", mime.as_ref())
            .body(Body::from(file.data.into_owned()))
            .unwrap())
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

pub fn ui_routes() -> axum::Router<AppState> {
    Router::new()
        // Root
        .route("/", get(root::index))
        // Users
        .route("/users", get(users::users_page))
        .route("/users", post(users::add_user))
        .route("/users/list", get(users::get_user_list))
        .route("/users/{id}", axum::routing::delete(users::delete_user))
        .route("/users/mappings", post(users::add_mapping))
        .route(
            "/users/{user_id}/mappings/{mapping_id}",
            axum::routing::delete(users::delete_mapping),
        )
        .route(
            "/users/{user_id}/sessions",
            axum::routing::delete(users::delete_sessions),
        )
        .route("/servers", get(servers::servers_page))
        .route("/servers", post(servers::add_server))
        .route("/servers/list", get(servers::get_server_list))
        .route(
            "/servers/{id}",
            axum::routing::delete(servers::delete_server),
        )
        .route(
            "/servers/{id}/priority",
            axum::routing::patch(servers::update_server_priority),
        )
        .route("/servers/{id}/status", get(servers::check_server_status))
        // Settings
        .route("/settings", get(settings::settings_page))
        .route("/settings/form", get(settings::settings_form))
        .route("/settings/save", post(settings::save_settings))
        .route("/settings/reload", post(settings::reload_config))
        .route_layer(login_required!(Backend, login_url = "/ui/login"))
        .route("/resources/{*path}", get(resource_handler))
        .merge(auth::router())
}
