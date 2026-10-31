//! Production adapter composition: route table, session/auth layers, URL-prefix
//! wrapping, and root monitoring endpoints. No listeners or background tasks.
use super::{
    assets::index_handler, auth_keys, branding, fallback::fallback_handler, quick_connect,
    sessions, state::AppState, syncplay,
};
use axum::{
    extract::{FromRef, Request},
    middleware,
    response::IntoResponse,
    routing::{any, get, post},
    Router,
};
use axum_login::{
    tower_sessions::{Expiry, SessionManagerLayer},
    AuthManagerLayerBuilder,
};
use axum_messages::MessagesManagerLayer;
use jellyswarrm_macros::lowercase_routes;
use std::net::SocketAddr;
use tower::ServiceBuilder;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tower_sessions::cookie::Key;
use tower_sessions_sqlx_store::SqliteStore;
use tracing::info;

use crate::{config::StartupConfig, ui::Backend};

/// Callers prepare/migrate storage and own task lifetimes. Tests and startup use
/// the same routes, middleware order, session policy, prefix and health wiring.
pub(crate) fn build_router(
    state: AppState,
    startup: &StartupConfig,
    pool: sqlx::SqlitePool,
    session_store: SqliteStore,
    addr: SocketAddr,
) -> Router {
    let session_layer = SessionManagerLayer::new(session_store)
        .with_secure(false)
        .with_same_site(tower_sessions::cookie::SameSite::Lax)
        .with_expiry(Expiry::OnInactivity(time::Duration::days(1)))
        .with_signed(Key::from(startup.session_key.as_slice()));
    let management = super::state::ManagementState::from_ref(&state);
    let backend = Backend::new(management.credentials, management.user_authorization);
    let auth_layer = AuthManagerLayerBuilder::new(backend, session_layer).build();

    let app = routes()
        .nest(&format!("/{}", startup.ui_route), crate::ui::ui_routes())
        .layer(
            ServiceBuilder::new()
                .layer(TraceLayer::new_for_http())
                .layer(CorsLayer::permissive()),
        )
        .layer(MessagesManagerLayer)
        .layer(middleware::from_fn_with_state(
            std::sync::Arc::<crate::auth::UserAuthorizationService>::from_ref(&state),
            super::middleware::enforce_read_only_api_keys,
        ))
        .layer(auth_layer)
        .with_state(state)
        .route("/GetUTCTime", get(syncplay::get_utc_time));

    wrap(
        app,
        startup.url_prefix.as_ref().map(ToString::to_string),
        addr,
        &startup.ui_route,
        pool,
    )
}

fn routes() -> Router<AppState> {
    lowercase_routes! {
        Router::new()
            .merge(sessions::router())
            .route("/", get(index_handler))
            .route("/QuickConnect/Enabled", get(quick_connect::enabled))
            .route("/QuickConnect/Initiate", post(quick_connect::initiate))
            .route("/QuickConnect/Connect", get(quick_connect::connect))
            .route("/QuickConnect/Authorize", post(quick_connect::authorize))
            .route("/Branding/Configuration", get(branding::handle_branding))
            .route("/websocket", get(sessions::websocket))
            .route("/socket", get(sessions::websocket))
            .route("/GetUtcTime", get(syncplay::get_utc_time))
            .route("/Auth/Keys", get(auth_keys::list_api_keys).post(auth_keys::create_api_key))
            .nest("/SyncPlay", Router::new()
                .route("/New", post(syncplay::create_group))
                .route("/Join", post(syncplay::join_group))
                .route("/Leave", post(syncplay::leave_group))
                .route("/List", get(syncplay::list_groups))
                .route("/{id}", get(syncplay::get_group))
                .route("/SetNewQueue", post(syncplay::set_new_queue))
                .route("/SetPlaylistItem", post(syncplay::set_playlist_item))
                .route("/RemoveFromPlaylist", post(syncplay::remove_from_playlist))
                .route("/MovePlaylistItem", post(syncplay::move_playlist_item))
                .route("/Queue", post(syncplay::queue_items))
                .route("/Unpause", post(syncplay::unpause))
                .route("/Pause", post(syncplay::pause))
                .route("/Stop", post(syncplay::stop))
                .route("/Seek", post(syncplay::seek))
                .route("/Buffering", post(syncplay::buffering))
                .route("/Ready", post(syncplay::ready))
                .route("/SetIgnoreWait", post(syncplay::set_ignore_wait))
                .route("/NextItem", post(syncplay::next_item))
                .route("/PreviousItem", post(syncplay::previous_item))
                .route("/SetRepeatMode", post(syncplay::set_repeat_mode))
                .route("/SetShuffleMode", post(syncplay::set_shuffle_mode))
                .route("/Ping", post(syncplay::ping)))
            .merge(super::client_routes::router())
            .route("/{*path}", any(fallback_handler))
            .fallback(fallback_handler)
    }
}

fn wrap(
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
    app.merge(super::health::router(pool))
}
