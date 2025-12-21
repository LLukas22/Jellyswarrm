use std::sync::LazyLock;
use axum::{
    body::Body,
    extract::Path,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use axum_login::login_required;
use hyper::StatusCode;
use rust_embed::RustEmbed;
use tracing::error;

use crate::{
    ui::auth::{AuthenticatedUser, UserRole},
    AppState, Asset,
};

pub mod admin;
pub mod auth;
pub mod root;
pub mod server_status;
pub mod user;
pub use auth::Backend;

#[derive(RustEmbed)]
#[folder = "src/ui/resources/"]
struct Resources;

async fn require_admin(
    AuthenticatedUser(user): AuthenticatedUser,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> impl IntoResponse {
    if user.role == UserRole::Admin {
        next.run(req).await
    } else {
        StatusCode::FORBIDDEN.into_response()
    }
}

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

#[derive(Debug, Clone, serde::Serialize)]
pub struct JellyfinUiVersion {
    pub version: String,
    pub commit: String,
}

fn get_jellyfin_ui_version() -> Option<JellyfinUiVersion> {
    if let Some(file) = Asset::get("ui-version.env") {
        let content = String::from_utf8_lossy(&file.data);
        let mut version = "unknown";
        let mut commit = "unknown";
        for line in content.lines() {
            if line.starts_with("UI_VERSION=") {
                version = line.trim_start_matches("UI_VERSION=");
            } else if line.starts_with("UI_COMMIT=") {
                commit = line.trim_start_matches("UI_COMMIT=");
            }
        }
        Some(JellyfinUiVersion {
            version: version.to_string(),
            commit: commit.to_string(),
        })
    } else {
        error!("Failed to load Jellyfin UI version info from embedded resources");
        None
    }
}

pub static JELLYFIN_UI_VERSION: LazyLock<Option<JellyfinUiVersion>> =
    LazyLock::new(get_jellyfin_ui_version);

pub fn ui_routes() -> axum::Router<AppState> {
    let admin_routes = Router::new()
        // Users
        .route("/users", get(admin::users::users_page))
        .route("/users", post(admin::users::add_user))
        .route("/users/list", get(admin::users::get_user_list))
        .route("/users/{id}/delete", post(admin::users::delete_user))
        .route("/users/mappings", post(admin::users::add_mapping))
        .route(
            "/users/{user_id}/mappings/{mapping_id}",
            axum::routing::delete(admin::users::delete_mapping),
        )
        .route(
            "/users/{user_id}/sessions",
            axum::routing::delete(admin::users::delete_sessions),
        )
        .route("/servers", get(admin::servers::servers_page))
        .route("/servers", post(admin::servers::add_server))
        .route("/servers/list", get(admin::servers::get_server_list))
        .route(
            "/servers/{id}",
            axum::routing::delete(admin::servers::delete_server),
        )
        .route(
            "/servers/{id}/priority",
            axum::routing::patch(admin::servers::update_server_priority),
        )
        .route(
            "/servers/{id}/admin",
            post(admin::servers::add_server_admin),
        )
        .route(
            "/servers/{id}/admin",
            axum::routing::delete(admin::servers::delete_server_admin),
        )
        // Settings
        .route("/settings", get(admin::settings::settings_page))
        .route("/settings/form", get(admin::settings::settings_form))
        .route("/settings/save", post(admin::settings::save_settings))
        .route("/settings/reload", post(admin::settings::reload_config))
        .route_layer(middleware::from_fn(require_admin));

    Router::new()
        // Root
        .route("/", get(root::index))
        .route("/user/servers", get(user::servers::get_user_servers))
        .route(
            "/user/servers/{id}",
            axum::routing::delete(user::servers::delete_server_mapping),
        )
        .route(
            "/user/servers/{id}/connect",
            post(user::servers::connect_server),
        )
        .route("/user/media", get(user::media::get_user_media))
        .route(
            "/user/media/server/{server_id}/libraries",
            get(user::media::get_server_libraries),
        )
        .route(
            "/user/media/server/{server_id}/library/{library_id}/items",
            get(user::media::get_library_items),
        )
        .route(
            "/user/media/image/{server_id}/{item_id}",
            get(user::media::proxy_media_image),
        )
        .route("/user/profile", get(user::profile::get_user_profile))
        .route(
            "/user/profile/password",
            post(user::profile::post_user_password),
        )
        .route(
            "/user/servers/{id}/status",
            get(user::servers::check_user_server_status),
        )
        .route(
            "/servers/{id}/status",
            get(server_status::check_server_status),
        )
        .merge(admin_routes)
        .route_layer(login_required!(Backend, login_url = "/ui/login"))
        .route("/resources/{*path}", get(resource_handler))
        .merge(auth::router())
}
