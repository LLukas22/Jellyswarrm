//! Serve the management UI, composing administrative and authenticated-user routes.
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

use crate::{
    http::state::AppState,
    ui::auth::{AuthenticatedUser, UserRole},
};

pub(crate) mod admin;
pub(crate) mod auth;
pub(crate) mod release_status;
pub(crate) mod root;
pub(crate) mod server_status;
pub(crate) mod user;
pub(crate) use auth::Backend;

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

pub(crate) fn ui_routes() -> axum::Router<AppState> {
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
            "/servers/{id}/media-streaming-mode",
            axum::routing::patch(admin::servers::update_server_media_streaming_mode),
        )
        .route(
            "/servers/{id}/admin",
            post(admin::servers::add_server_admin),
        )
        .route(
            "/servers/{id}/admin",
            axum::routing::delete(admin::servers::delete_server_admin),
        )
        .route("/libraries", get(admin::libraries::libraries_page))
        .route(
            "/libraries/list",
            get(admin::libraries::library_groups_list),
        )
        .route(
            "/libraries/merge-libraries",
            axum::routing::patch(admin::libraries::update_merge_libraries),
        )
        .route("/libraries/groups", post(admin::libraries::create_group))
        .route(
            "/libraries/groups/{virtual_id}",
            axum::routing::delete(admin::libraries::delete_group),
        )
        .route("/libraries/assign", post(admin::libraries::assign_library))
        .route(
            "/libraries/groups/{group_virtual_id}/remove",
            post(admin::libraries::remove_member),
        )
        .route(
            "/libraries/groups/{virtual_id}/rename",
            post(admin::libraries::rename_group),
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
        .route(
            "/release-status",
            get(release_status::jellyswarrm_release_status),
        )
        .route("/user/servers", get(user::servers::get_user_servers))
        .route(
            "/user/servers/{id}",
            axum::routing::delete(user::servers::delete_server_mapping),
        )
        .route(
            "/user/servers/{id}/connect",
            post(user::servers::connect_server),
        )
        .route(
            "/user/servers/{id}/quick-connect",
            post(user::servers::initiate_quick_connect),
        )
        .route(
            "/user/servers/{id}/quick-connect/{request_id}",
            post(user::servers::finish_quick_connect),
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
