//! Compose concrete services, session/authentication layers, background tasks, and the HTTP listener.
#[cfg(debug_assertions)]
use super::debug_initialization;
use super::shutdown::shutdown_signal;
use crate::{
    libraries::VirtualLibraryService, media::MediaStorageService, servers::ServerStorageService,
};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tower_sessions_sqlx_store::SqliteStore;
use tracing::{error, info, warn};
pub(crate) async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let paths = crate::config::Paths::resolve()?;
    let _logging_guard = super::logging::initialize(&paths);

    let loaded_config = crate::config::load_config(&paths)?;
    let startup = loaded_config.startup_config();
    info!("Loaded configuration: {:?}", loaded_config);

    let pool = crate::persistence::database::connect(&paths).await?;

    // Create reqwest client for regular API traffic.
    let reqwest_client = reqwest::Client::builder()
        .user_agent(concat!(
            "Jellyswarrm API Client/",
            env!("CARGO_PKG_VERSION")
        ))
        .timeout(Duration::from_secs(startup.timeout))
        .build()
        .unwrap_or_else(|e| {
            error!("Failed to create reqwest client: {}", e);
            std::process::exit(1);
        });

    // Create a dedicated client for proxied media streams.
    // Avoid a global request timeout on long-lived responses and disable automatic
    // response decompression so we forward bytes as-is with less CPU overhead.
    let streaming_reqwest_client = reqwest::Client::builder()
        .user_agent(concat!(
            "Jellyswarrm API Client/",
            env!("CARGO_PKG_VERSION")
        ))
        .connect_timeout(Duration::from_secs(startup.timeout))
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
    let user_authorization =
        super::authorization::build_authorization(pool.clone(), &loaded_config).await?;

    // Initialize server storage service
    let server_storage = ServerStorageService::new(pool.clone());

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

    let app_state = super::composition::adapter_state(
        super::composition::HttpClients {
            api: reqwest_client,
            streaming: streaming_reqwest_client,
        },
        loaded_config.clone(),
        paths,
        Arc::new(user_authorization.clone()),
        Arc::new(server_storage.clone()),
        Arc::new(media_storage),
        Arc::new(virtual_library_service),
    );

    let session_store = SqliteStore::new(pool.clone());
    session_store.migrate().await?;

    let runtime = super::runtime::ApplicationRuntime::start(
        server_storage,
        axum::extract::FromRef::from_ref(&app_state),
        session_store.clone(),
        startup.server_background_check_interval_secs,
    );

    // Create socket address
    let addr = match format!("{}:{}", startup.host, startup.port).parse::<SocketAddr>() {
        Ok(addr) => addr,
        Err(e) => {
            error!(
                "Invalid address {}:{}: {}",
                loaded_config.host, loaded_config.port, e
            );
            std::process::exit(1);
        }
    };

    let app = super::router::build_router(app_state, &startup, pool, session_store, addr);

    // Start the server
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            error!("Failed to bind to {}: {}", addr, e);
            std::process::exit(1);
        }
    };

    axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    runtime.shutdown().await;
    Ok(())
}
