//! Production client routes, item-detail methods, URL-prefix wrapping, and root monitoring endpoints.
use super::{assets::index_handler, proxy::proxy_handler};
use crate::{sessions, ui::ui_routes, AppState};
use axum::{
    extract::Request,
    response::IntoResponse,
    routing::{any, get, post},
    Router,
};
use jellyswarrm_macros::lowercase_routes;
use std::net::SocketAddr;
use tracing::info;
pub(crate) fn routes(ui_route: &str) -> Router<AppState> {
    lowercase_routes! {
        Router::new()
            .merge(sessions::router())
            // UI Management routes
            .nest(&format!("/{ui_route}"), ui_routes())
            .route("/", get(index_handler))
            .route(
                "/QuickConnect/Enabled",
                get(crate::http::handlers::quick_connect::handle_quick_connect_enabled),
            )
            .route(
                "/QuickConnect/Initiate",
                post(crate::http::handlers::quick_connect::handle_quick_connect_initiate),
            )
            .route(
                "/QuickConnect/Connect",
                get(crate::http::handlers::quick_connect::handle_quick_connect_connect),
            )
            .route(
                "/QuickConnect/Authorize",
                post(crate::http::handlers::quick_connect::handle_quick_connect_authorize),
            )
            .route(
                "/Branding/Configuration",
                get(crate::http::handlers::branding::handle_branding),
            )
            .route("/websocket", get(sessions::websocket))
            .route("/socket", get(sessions::websocket))
            .route("/GetUtcTime", get(crate::http::handlers::syncplay::get_utc_time))
            .route(
                "/Auth/Keys",
                get(crate::http::handlers::auth_keys::list_api_keys)
                    .post(crate::http::handlers::auth_keys::create_api_key),
            )
            .nest(
                "/SyncPlay",
                Router::new()
                    .route("/New", post(crate::http::handlers::syncplay::create_group))
                    .route("/Join", post(crate::http::handlers::syncplay::join_group))
                    .route("/Leave", post(crate::http::handlers::syncplay::leave_group))
                    .route("/List", get(crate::http::handlers::syncplay::list_groups))
                    .route("/{id}", get(crate::http::handlers::syncplay::get_group))
                    .route("/SetNewQueue", post(crate::http::handlers::syncplay::set_new_queue))
                    .route("/SetPlaylistItem", post(crate::http::handlers::syncplay::set_playlist_item))
                    .route(
                        "/RemoveFromPlaylist",
                        post(crate::http::handlers::syncplay::remove_from_playlist),
                    )
                    .route(
                        "/MovePlaylistItem",
                        post(crate::http::handlers::syncplay::move_playlist_item),
                    )
                    .route("/Queue", post(crate::http::handlers::syncplay::queue_items))
                    .route("/Unpause", post(crate::http::handlers::syncplay::unpause))
                    .route("/Pause", post(crate::http::handlers::syncplay::pause))
                    .route("/Stop", post(crate::http::handlers::syncplay::stop))
                    .route("/Seek", post(crate::http::handlers::syncplay::seek))
                    .route("/Buffering", post(crate::http::handlers::syncplay::buffering))
                    .route("/Ready", post(crate::http::handlers::syncplay::ready))
                    .route("/SetIgnoreWait", post(crate::http::handlers::syncplay::set_ignore_wait))
                    .route("/NextItem", post(crate::http::handlers::syncplay::next_item))
                    .route("/PreviousItem", post(crate::http::handlers::syncplay::previous_item))
                    .route("/SetRepeatMode", post(crate::http::handlers::syncplay::set_repeat_mode))
                    .route("/SetShuffleMode", post(crate::http::handlers::syncplay::set_shuffle_mode))
                    .route("/Ping", post(crate::http::handlers::syncplay::ping)),
            )
            .merge(crate::http::client_routes::router())
            .route("/{*path}", any(proxy_handler))
            .fallback(proxy_handler)
    }
}

pub(crate) fn wrap(
    app: Router,
    url_prefix: Option<String>,
    addr: SocketAddr,
    ui_route: &str,
    pool: sqlx::SqlitePool,
) -> Router {
    let app = if let Some(url_prefix) = url_prefix {
        info!("Using URL prefix: {}", url_prefix);

        info!("Starting reverse proxy on http://{}/{}", addr, url_prefix);
        info!(
            "UI Management routes available at: http://{}/{}/{}",
            addr,
            url_prefix,
            ui_route.trim_start_matches('/')
        );

        Router::new()
            .nest(&format!("/{}", url_prefix), app)
            .fallback(
                // Redirect any request outside the prefixed subtree into the prefixed route,
                // preserving the original path. e.g. /foo/bar -> /{url_prefix}/foo/bar
                // capture url_prefix by value
                {
                    let prefix = url_prefix;
                    move |req: Request| {
                        let prefix = prefix.clone();
                        async move {
                            let orig = req.uri().path().trim_end_matches("/");
                            let prefix_slash = format!("/{}", prefix);
                            let target = if orig.starts_with(&prefix_slash) {
                                // already has prefix - avoid double-appending
                                orig
                            } else {
                                &format!("{prefix_slash}{orig}")
                            };
                            axum::response::Redirect::temporary(target).into_response()
                        }
                    }
                },
            )
    } else {
        info!("No URL prefix configured, using root path");
        info!("Starting reverse proxy on http://{}", addr);
        info!(
            "UI Management routes available at: http://{}/{}",
            addr, ui_route
        );
        app
    };

    // Keep monitoring outside the URL prefix and authentication/session middleware.
    app.merge(crate::http::health::router(pool))
}
// Shared with HTTP integration tests so mutations exercise the production route.
pub(crate) fn item_detail_routes() -> axum::routing::MethodRouter<AppState> {
    get(crate::http::handlers::items::get_item)
        .post(proxy_handler)
        .delete(proxy_handler)
}
