//! Production adapter composition, without listeners or background tasks.
use std::net::SocketAddr;

use axum::{extract::FromRef, middleware, routing::get, Router};
use axum_login::{
    tower_sessions::{Expiry, SessionManagerLayer},
    AuthManagerLayerBuilder,
};
use axum_messages::MessagesManagerLayer;
use tower::ServiceBuilder;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tower_sessions::cookie::Key;
use tower_sessions_sqlx_store::SqliteStore;

use crate::{
    config::StartupConfig,
    http::{
        middleware::enforce_read_only_api_keys,
        state::{AppState, ManagementState},
    },
    ui::Backend,
};

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
    let management = ManagementState::from_ref(&state);
    let backend = Backend::new(management.credentials, management.user_authorization);
    let auth_layer = AuthManagerLayerBuilder::new(backend, session_layer).build();

    let app = crate::http::router::routes()
        .nest(&format!("/{}", startup.ui_route), crate::ui::ui_routes())
        .layer(
            ServiceBuilder::new()
                .layer(TraceLayer::new_for_http())
                .layer(CorsLayer::permissive()),
        )
        .layer(MessagesManagerLayer)
        .layer(middleware::from_fn_with_state(
            std::sync::Arc::<crate::auth::UserAuthorizationService>::from_ref(&state),
            enforce_read_only_api_keys,
        ))
        .layer(auth_layer)
        .with_state(state)
        .route(
            "/GetUTCTime",
            get(crate::http::handlers::syncplay::get_utc_time),
        );

    crate::http::router::wrap(
        app,
        startup.url_prefix.as_ref().map(ToString::to_string),
        addr,
        &startup.ui_route,
        pool,
    )
}
