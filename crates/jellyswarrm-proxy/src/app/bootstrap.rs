//! Compose concrete services, session/authentication layers, background tasks, and the HTTP listener.
#[cfg(debug_assertions)]
use super::debug_initialization;
use super::shutdown::shutdown_signal;
use crate::{
    application::playback::sessions::SessionStorage,
    auth::UserAuthorizationService,
    http::{handlers::quick_connect, middleware::enforce_read_only_api_keys},
    libraries::VirtualLibraryService,
    media::MediaStorageService,
    processors::ProxyProcessors,
    servers::ServerStorageService,
    ui::Backend,
    AppState, DataContext,
};
use axum::{middleware, routing::get};
use axum_login::{
    tower_sessions::{ExpiredDeletion, Expiry, SessionManagerLayer},
    AuthManagerLayerBuilder,
};
use axum_messages::MessagesManagerLayer;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tower::ServiceBuilder;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tower_sessions::cookie::Key;
use tower_sessions_sqlx_store::SqliteStore;
use tracing::{error, info, warn};
pub(crate) async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let _logging_guard = super::logging::initialize();

    let loaded_config = crate::config::load_config()?;
    info!("Loaded configuration: {:?}", loaded_config);

    let pool = crate::persistence::database::connect().await?;

    // Create reqwest client for regular API traffic.
    let reqwest_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(loaded_config.timeout))
        .build()
        .unwrap_or_else(|e| {
            error!("Failed to create reqwest client: {}", e);
            std::process::exit(1);
        });

    // Create a dedicated client for proxied media streams.
    // Avoid a global request timeout on long-lived responses and disable automatic
    // response decompression so we forward bytes as-is with less CPU overhead.
    let streaming_reqwest_client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(loaded_config.timeout))
        .tcp_nodelay(true)
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .build()
        .unwrap_or_else(|e| {
            error!("Failed to create streaming reqwest client: {}", e);
            std::process::exit(1);
        });

    // Initialize user authorization service
    let mapping_key = crate::auth::encryption::MappingEncryptionKey::from_session_key(
        &loaded_config.session_key,
    )?;
    let mut user_authorization = UserAuthorizationService::with_mapping_key(
        pool.clone(),
        mapping_key,
        (&loaded_config.password).into(),
    );
    user_authorization
        .enable_session_encryption(&loaded_config.session_key)
        .await?;

    // Initialize server storage service
    let server_storage = ServerStorageService::new(pool.clone());
    server_storage.start_health_check_loop(loaded_config.server_background_check_interval_secs);

    // Initialize media storage service
    let media_storage = MediaStorageService::new(pool.clone());

    let virtual_library_service =
        VirtualLibraryService::new(pool.clone(), server_storage.clone(), media_storage.clone());

    #[cfg(debug_assertions)]
    let mut debug_server_ids = Vec::with_capacity(loaded_config.preconfigured_servers.len());

    if !loaded_config.preconfigured_servers.is_empty() {
        info!(
            "Configuring {} preconfigured servers from config",
            loaded_config.preconfigured_servers.len()
        );
        for server in &loaded_config.preconfigured_servers {
            match server_storage
                .upsert_preconfigured_server(
                    &server.name,
                    &server.url,
                    server.priority,
                    server.media_streaming_mode,
                )
                .await
            {
                Ok(_server_id) => {
                    info!(
                        "  Configured server: {} ({}) with priority {}",
                        server.name, server.url, server.priority
                    );
                    #[cfg(debug_assertions)]
                    debug_server_ids.push(_server_id);
                }
                Err(e) => {
                    error!(
                        "  Failed to configure server {} ({}): {}",
                        server.name, server.url, e
                    );
                }
            }
        }
    }

    #[cfg(debug_assertions)]
    if let Some(debug_user) = &loaded_config.debug_user {
        let mapping_count = debug_initialization::initialize_debug_user(
            debug_user,
            &debug_server_ids,
            &user_authorization,
            &server_storage,
        )
        .await?;
        info!(
            "Configured debug user '{}' with {} server mappings",
            debug_user.username, mapping_count
        );
    }

    match server_storage.list_servers().await {
        Ok(servers) => {
            if servers.is_empty() {
                warn!("No servers found, configure them via the UI.");
            } else {
                info!("Found {} configured servers", servers.len());
                for server in &servers {
                    info!(
                        "  {} ({}): priority {}",
                        server.name, server.url, server.priority,
                    );
                }
            }
        }
        Err(e) => {
            error!("Failed to check existing servers: {}", e);
        }
    }

    let data_context = DataContext {
        user_authorization: Arc::new(user_authorization.clone()),
        server_storage: Arc::new(server_storage.clone()),
        media_storage: Arc::new(media_storage.clone()),
        virtual_library_service: Arc::new(virtual_library_service),
        play_sessions: Arc::new(SessionStorage::new()),
        config: Arc::new(tokio::sync::RwLock::new(loaded_config.clone())),
    };

    let proxy_processors = ProxyProcessors::new(data_context.clone());

    let app_state = AppState::new(
        reqwest_client,
        streaming_reqwest_client,
        data_context,
        proxy_processors,
        quick_connect::QuickConnectStorage::new(),
    );

    quick_connect::QuickConnectStorage::start_cleanup_task(app_state.quick_connect.clone());

    let session_store = SqliteStore::new(pool.clone());
    session_store.migrate().await?;

    let deletion_task = tokio::task::spawn(
        session_store
            .clone()
            .continuously_delete_expired(tokio::time::Duration::from_secs(60)),
    );

    let key = Key::from(loaded_config.session_key.as_slice());

    let session_layer = SessionManagerLayer::new(session_store)
        .with_secure(false)
        .with_same_site(tower_sessions::cookie::SameSite::Lax)
        .with_expiry(Expiry::OnInactivity(time::Duration::days(1))) // 24 hour
        .with_signed(key);

    let backend = Backend::new(
        app_state.config.clone(),
        app_state.user_authorization.clone(),
    );
    let auth_layer = AuthManagerLayerBuilder::new(backend, session_layer).build();

    let ui_route = loaded_config.ui_route.to_string();

    let app = crate::http::router::routes(&ui_route)
        .layer(
            ServiceBuilder::new()
                .layer(TraceLayer::new_for_http())
                .layer(CorsLayer::permissive()),
        )
        .layer(MessagesManagerLayer)
        .layer(middleware::from_fn_with_state(
            app_state.clone(),
            enforce_read_only_api_keys,
        ))
        .layer(auth_layer)
        .with_state(app_state)
        .route(
            "/GetUTCTime",
            get(crate::http::handlers::syncplay::get_utc_time),
        );

    // Create socket address
    let addr = match format!("{}:{}", loaded_config.host, loaded_config.port).parse::<SocketAddr>()
    {
        Ok(addr) => addr,
        Err(e) => {
            error!(
                "Invalid address {}:{}: {}",
                loaded_config.host, loaded_config.port, e
            );
            std::process::exit(1);
        }
    };

    let app = crate::http::router::wrap(
        app,
        loaded_config.url_prefix.map(|p| p.to_string()),
        addr,
        &ui_route,
        pool,
    );

    // Start the server
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            error!("Failed to bind to {}: {}", addr, e);
            std::process::exit(1);
        }
    };

    axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(shutdown_signal(deletion_task.abort_handle()))
        .await?;

    deletion_task.await??;
    Ok(())
}
